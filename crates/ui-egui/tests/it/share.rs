//! Share is Nextcloud sharing for the open workbook, and nothing else: every entry point runs the
//! one Share function, which asks the hosting page, opens the browser at the file's sharing, or
//! says "No Nextcloud connected". It never exports or opens a dialog.

use std::sync::{Arc, Mutex};

use egui::Key;
use egui_kittest::kittest::Queryable as _;
use gridcraft_ui_egui::craft_share::{self, ShareRoute};
use gridcraft_ui_egui::{Services, SheetApp, control};
use serde_json::json;

type Harness = egui_kittest::Harness<'static, SheetApp>;

/// What Share did: page posts, URLs opened, files delivered (downloads or host saves).
#[derive(Default)]
struct Calls {
    posts: usize,
    urls: Vec<String>,
    delivered: Vec<String>,
}

type Log = Arc<Mutex<Calls>>;

fn deliveries(log: &Log) -> Services {
    let (d, u) = (log.clone(), log.clone());
    Services {
        download: Some(Box::new(move |name: &str, _: &[u8]| d.lock().unwrap().delivered.push(name.into()))),
        open_url: Some(Box::new(move |url: &str| u.lock().unwrap().urls.push(url.into()))),
        ..Default::default()
    }
}

/// Framed by a Nextcloud page that shares files ([`host_open`] opens its file).
fn web_host(log: &Log) -> Services {
    let p = log.clone();
    Services {
        host_share: Some(Box::new(move || {
            p.lock().unwrap().posts += 1;
            Ok(())
        })),
        ..deliveries(log)
    }
}

/// A desktop whose Nextcloud client syncs `/home/u/Nextcloud` as the account's root.
fn desktop(log: &Log) -> Services {
    Services {
        share_route: Some(Box::new(|path: &std::path::Path| match path.strip_prefix("/home/u/Nextcloud") {
            Ok(rest) => ShareRoute::NextcloudDesktop {
                server: "https://cloud.example.test".into(),
                user: "u".into(),
                remote_path: format!("/{}", rest.to_string_lossy()),
            },
            Err(_) => ShareRoute::None,
        })),
        ..deliveries(log)
    }
}

fn harness(services: Services, path: Option<&str>) -> Harness {
    let mut h = crate::support::services_harness(services, 4);
    h.state_mut().session.active_mut().unwrap().path = path.map(str::to_string);
    h
}

/// The page sends `name` (an XLSX workbook), as the web bridge does: the workbook it opens
/// becomes the page's. Returns its id.
fn host_open(h: &mut Harness, name: &str) -> u64 {
    let before: Vec<u64> = h.state().session.documents().iter().map(|d| d.uid).collect();
    // A file with content: an untouched blank workbook would be replaced by the next open.
    h.state_mut().run("cell.set", json!({"cell": "A1", "input": "42"})).unwrap();
    let bytes = h.state_mut().run("file.saveBytes", json!({})).unwrap();
    h.state_mut().run("file.open", json!({"name": name, "base64": bytes["base64"]})).unwrap();
    h.run_steps(2);
    assert!(h.state_mut().note_host_open(name, &before), "{name} opened");
    h.run_steps(2);
    h.state().session.active().unwrap().uid
}

fn no_nextcloud_shown(h: &Harness) -> bool {
    h.state().toast.as_ref().is_some_and(|(t, _)| t == craft_share::NO_NEXTCLOUD)
}

/// Nothing but sharing happened: no copy delivered, no dialog, no Backstage page, no alert.
fn nothing_else(h: &Harness, log: &Log) {
    assert!(log.lock().unwrap().delivered.is_empty(), "Share delivers no file");
    assert!(h.state().dialog.is_none(), "Share opens no dialog");
    assert!(h.state().backstage.is_none(), "Share opens no Backstage page");
    assert!(h.state().message.is_none(), "Share shows no message box");
}

fn click_title_bar_share(h: &mut Harness) {
    h.get_by_role_and_label(egui::accesskit::Role::Button, "Share").click();
    h.run_steps(3);
}

fn command_search_share(h: &mut Harness) {
    h.state_mut().open_dialog("commandSearch", json!({}));
    h.run_steps(2);
    h.state_mut().dialog.as_mut().unwrap().search = "file.share".into();
    h.run_steps(2);
    h.key_press(Key::Enter);
    h.run_steps(3);
}

fn control_share(h: &mut Harness) -> serde_json::Value {
    let ctx = h.ctx.clone();
    let control::Outcome::Done(response) = control::handle(h.state_mut(), &ctx, "file.share", &json!({})) else {
        panic!("file.share answers");
    };
    h.run_steps(2);
    response
}

#[test]
fn every_entry_point_asks_the_hosting_page() {
    let log = Log::default();
    let mut h = harness(web_host(&log), None);
    host_open(&mut h, "Budget.xlsx");
    click_title_bar_share(&mut h);
    assert_eq!(log.lock().unwrap().posts, 1, "the title bar's Share asks the page");
    command_search_share(&mut h);
    assert_eq!(log.lock().unwrap().posts, 2, "command search's Share asks the page");
    let response = control_share(&mut h);
    assert_eq!(response["ok"], true, "{response}");
    assert_eq!(log.lock().unwrap().posts, 3, "the control channel's Share asks the page");
    assert!(!no_nextcloud_shown(&h));
    nothing_else(&h, &log);
}

#[test]
fn every_entry_point_says_no_nextcloud_without_one() {
    // A plain web build: downloads, no hosting page.
    let log = Log::default();
    let mut h = harness(deliveries(&log), Some("Budget.xlsx"));
    click_title_bar_share(&mut h);
    assert!(no_nextcloud_shown(&h), "the title bar's Share says so");
    h.state_mut().toast = None;
    command_search_share(&mut h);
    assert!(no_nextcloud_shown(&h), "command search's Share says so");
    h.state_mut().toast = None;
    let response = control_share(&mut h);
    assert_eq!(response["result"]["shared"], false, "automation hears it did not share: {response}");
    assert!(no_nextcloud_shown(&h));
    assert!(log.lock().unwrap().urls.is_empty() && log.lock().unwrap().posts == 0);
    nothing_else(&h, &log);
}

#[test]
fn the_page_shares_only_the_workbook_it_opened() {
    let log = Log::default();
    // Before the page sent a file: the blank start workbook is not the page's.
    let mut h = harness(web_host(&log), None);
    click_title_bar_share(&mut h);
    assert_eq!(log.lock().unwrap().posts, 0, "a workbook the page never sent");
    assert!(no_nextcloud_shown(&h));
    let page = host_open(&mut h, "Budget.xlsx");
    // A local workbook with the same name, opened in the frame (File › Open, a drop).
    let bytes = h.state_mut().run("file.saveBytes", json!({})).unwrap();
    h.state_mut().run("file.open", json!({"name": "Budget.xlsx", "base64": bytes["base64"]})).unwrap();
    h.run_steps(2);
    assert_ne!(h.state().session.active().unwrap().uid, page);
    h.state_mut().toast = None;
    click_title_bar_share(&mut h);
    assert_eq!(log.lock().unwrap().posts, 0, "a same-named workbook is not the page's");
    assert!(no_nextcloud_shown(&h));
    nothing_else(&h, &log);
    // The page's workbook, renamed by a conflict copy, is still the page's.
    h.state_mut().run("window.activate", json!({"doc": page})).unwrap();
    h.state_mut().session.active_mut().unwrap().title = "Budget (conflict).xlsx".into();
    h.run_steps(2);
    click_title_bar_share(&mut h);
    assert_eq!(log.lock().unwrap().posts, 1, "the page's workbook, whatever its name");
}

#[test]
fn a_second_file_from_the_page_does_not_take_over_share() {
    let log = Log::default();
    let mut h = harness(web_host(&log), None);
    let first = host_open(&mut h, "Budget.xlsx");
    h.state_mut().toast = None;
    let second = host_open(&mut h, "Other.xlsx");
    assert_ne!(first, second);
    let toast = h.state().toast.as_ref().map(|(t, _)| t.clone()).unwrap_or_default();
    assert!(toast.contains("Budget.xlsx"), "a toast says which file Share is for: {toast:?}");
    click_title_bar_share(&mut h);
    assert_eq!(log.lock().unwrap().posts, 0, "the second file is not the page's");
    h.state_mut().run("window.activate", json!({"doc": first})).unwrap();
    h.run_steps(2);
    click_title_bar_share(&mut h);
    assert_eq!(log.lock().unwrap().posts, 1, "the first file still is");
}

#[test]
fn a_page_that_cannot_be_reached_is_reported() {
    let log = Log::default();
    let services = Services { host_share: Some(Box::new(|| Err("no same-origin parent".into()))), ..deliveries(&log) };
    let mut h = harness(services, None);
    host_open(&mut h, "Budget.xlsx");
    let response = control_share(&mut h);
    assert_eq!(response["ok"], false, "a failed post is not a share: {response}");
    assert!(log.lock().unwrap().delivered.is_empty());
}

#[test]
fn desktop_opens_the_files_sharing_in_the_browser() {
    let log = Log::default();
    let mut h = harness(desktop(&log), Some("/home/u/Nextcloud/Team/Budget.xlsx"));
    click_title_bar_share(&mut h);
    let expected = craft_share::sharing_url("https://cloud.example.test", "/Team/Budget.xlsx");
    assert_eq!(log.lock().unwrap().urls, vec![expected], "the browser opens Nextcloud sharing for that file");
    assert!(!no_nextcloud_shown(&h));
    nothing_else(&h, &log);
}

#[test]
fn desktop_without_nextcloud_says_so() {
    let log = Log::default();
    // Outside every synced folder, and a workbook never saved.
    for path in [Some("/home/u/Documents/Budget.xlsx"), None] {
        let mut h = harness(desktop(&log), path);
        click_title_bar_share(&mut h);
        assert!(no_nextcloud_shown(&h), "{path:?}");
        assert!(log.lock().unwrap().urls.is_empty(), "{path:?} opens nothing");
        nothing_else(&h, &log);
    }
}

#[test]
fn the_exports_stay_in_the_more_menu() {
    let log = Log::default();
    let mut h = harness(deliveries(&log), None);
    h.get_by_label("More commands").click();
    h.run_steps(3);
    h.get_by_label("Export").click();
    h.run_steps(3);
    h.get_by_label("Export as CSV…").click();
    h.run_steps(3);
    assert_eq!(h.state().dialog.as_ref().map(|d| d.name.as_str()), Some("saveCopy"), "Export › CSV still saves a copy");
}

#[test]
fn the_desktop_route_is_looked_up_once_per_workbook_and_path() {
    let lookups = Arc::new(Mutex::new(Vec::<String>::new()));
    let seen = lookups.clone();
    let services = Services {
        share_route: Some(Box::new(move |path: &std::path::Path| {
            seen.lock().unwrap().push(path.to_string_lossy().into_owned());
            ShareRoute::None
        })),
        ..Default::default()
    };
    let mut h = harness(services, Some("/home/u/Nextcloud/A.xlsx"));
    h.run_steps(10);
    assert_eq!(lookups.lock().unwrap().len(), 1, "not on every frame");
    // Save As to another path: looked up again, once.
    h.state_mut().session.active_mut().unwrap().path = Some("/home/u/Nextcloud/B.xlsx".into());
    h.run_steps(10);
    click_title_bar_share(&mut h);
    assert_eq!(*lookups.lock().unwrap(), ["/home/u/Nextcloud/A.xlsx", "/home/u/Nextcloud/B.xlsx"]);
}
