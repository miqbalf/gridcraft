//! Save As = Export: every Save As and Export entry point runs the one Save As function, which
//! delivers a copy of the workbook. On the web, framed by a Nextcloud page or not, the copy
//! downloads and is never handed to the page; on the desktop the save dialog picks where it goes.
//! The workbook keeps its file, name and unsaved state.

use std::sync::{Arc, Mutex};

use egui::{Key, Modifiers};
use egui_kittest::kittest::Queryable as _;
use gridcraft_engine::{core::CellRef, core::Value};
use gridcraft_ui_egui::craft_share::export::Runtime;
use gridcraft_ui_egui::{SAVE_AS, Services, SheetApp, control};
use serde_json::json;

type Harness = egui_kittest::Harness<'static, SheetApp>;

/// What reached the outside: copies downloaded (name, MIME, bytes) and plain saves handed to the
/// page or browser (name).
#[derive(Default)]
struct Out {
    copies: Vec<(String, String, Vec<u8>)>,
    saves: Vec<String>,
}

type Log = Arc<Mutex<Out>>;

fn web(log: &Log, runtime: Option<Runtime>) -> Services {
    let (c, s) = (log.clone(), log.clone());
    Services {
        runtime,
        download_copy: Some(Box::new(move |name: &str, mime: &str, bytes: &[u8]| {
            c.lock().unwrap().copies.push((name.into(), mime.into(), bytes.to_vec()));
            Ok(())
        })),
        download: Some(Box::new(move |name: &str, _: &[u8]| s.lock().unwrap().saves.push(name.into()))),
        ..Default::default()
    }
}

fn harness(services: Services) -> Harness {
    let mut h = crate::support::services_harness(services, 4);
    h.state_mut().run("cell.set", json!({"cell": "A1", "input": "42"})).unwrap();
    h
}

/// The workbook's file, title and unsaved state, which no Save As may change.
fn doc_state(h: &Harness) -> (Option<String>, String, bool) {
    let d = h.state().session.active().unwrap();
    (d.path.clone(), d.display_title(), d.is_dirty())
}

fn page(h: &Harness) -> Option<&'static str> {
    h.state().backstage.as_ref().map(|b| b.page.id())
}

fn click(h: &mut Harness, label: &str) {
    h.get_by_role_and_label(egui::accesskit::Role::Button, label).click();
    h.run_steps(3);
}

fn command_search(h: &mut Harness, id: &str) {
    h.state_mut().open_dialog("commandSearch", json!({}));
    h.run_steps(2);
    h.state_mut().dialog.as_mut().unwrap().search = id.into();
    h.run_steps(2);
    h.key_press(Key::Enter);
    h.run_steps(3);
}

fn control_run(h: &mut Harness, id: &str, params: serde_json::Value) {
    let ctx = h.ctx.clone();
    let _ = control::handle(h.state_mut(), &ctx, id, &params);
    h.run_steps(2);
}

fn title_bar_export(h: &mut Harness, item: &str) {
    h.get_by_label("More commands").click();
    h.run_steps(3);
    h.get_by_label("Export").click();
    h.run_steps(3);
    h.get_by_label(item).click();
    h.run_steps(3);
}

/// Choose `id` on the File tab's Save As page and press its button.
fn save_as_page(h: &mut Harness, id: &'static str, button: &str) {
    h.state_mut().run("ui.backstage", json!({"page": "saveAs"})).unwrap();
    h.run_steps(2);
    h.state_mut().backstage.as_mut().unwrap().save_format = id;
    h.run_steps(2);
    click(h, button);
}

/// The copy delivered last has format `ext`'s extension and MIME type and holds the cell.
fn last_copy_is(log: &Log, ext: &str) {
    let out = log.lock().unwrap();
    let (name, mime, bytes) = out.copies.last().unwrap_or_else(|| panic!("no copy delivered for .{ext}"));
    let format = SAVE_AS.for_ext(ext).unwrap();
    assert!(name.ends_with(&format!(".{ext}")), "{name} is named for .{ext}");
    assert_eq!(mime, format.mime, "{name} downloads with the format's MIME type");
    assert!(!bytes.is_empty());
}

#[test]
fn the_list_is_valid_and_lists_every_format_the_engine_writes() {
    assert_eq!(SAVE_AS.check(), Ok(()));
    let mut s = gridcraft_engine::Session::new();
    s.new_workbook();
    let wb = s.doc().unwrap().wb.clone();
    // One extension per file type the engine knows, read-only types included, plus the print.
    let mut writes: Vec<&str> = ["xlsx", "xls", "xlsb", "ods", "csv", "tsv", "json", "html", "slk"]
        .into_iter()
        .filter(|ext| gridcraft_engine::io::save_bytes(&wb, &format!("Book.{ext}")).is_ok())
        .collect();
    if s.execute("file.exportPdf", json!({})).is_ok() {
        writes.push("pdf");
    }
    let mut listed: Vec<&str> = SAVE_AS.formats().iter().map(|f| f.ext).collect();
    writes.sort_unstable();
    listed.sort_unstable();
    assert_eq!(listed, writes, "Save As lists exactly the formats GridCraft writes");
}

#[test]
fn on_the_web_every_save_as_and_export_entry_point_downloads_a_copy() {
    let log = Log::default();
    let mut h = harness(web(&log, Some(Runtime::Web)));
    let before = doc_state(&h);
    for f in SAVE_AS.formats() {
        save_as_page(&mut h, f.id, &format!("Download (.{})", f.ext));
        last_copy_is(&log, f.ext);
        assert_eq!(page(&h), None, "the page closes once the copy is delivered");
    }
    for (label, ext) in [("Create PDF", "pdf"), ("CSV (Comma delimited)", "csv"), ("Web Page (.html)", "html")] {
        h.state_mut().run("ui.backstage", json!({"page": "export"})).unwrap();
        h.run_steps(2);
        click(&mut h, label);
        last_copy_is(&log, ext);
    }
    for (item, ext) in [
        ("Export as PDF…", "pdf"),
        ("Save a Copy as Excel Workbook (.xlsx)…", "xlsx"),
        ("Export as CSV…", "csv"),
        ("Export as Web Page (.html)…", "html"),
    ] {
        title_bar_export(&mut h, item);
        last_copy_is(&log, ext);
    }
    // The macOS menu bar's and the palette's commands (menu-style, `run_or_alert`).
    for (id, ext) in [("file.exportPdf", "pdf"), ("file.exportCsv", "csv"), ("file.exportHtml", "html")] {
        h.state_mut().run_or_alert(id, json!({}));
        last_copy_is(&log, ext);
        command_search(&mut h, id);
        last_copy_is(&log, ext);
    }
    // The control channel: Save As in a type, and automation's PDF (bytes back, and a copy).
    control_run(&mut h, "file.saveAs", json!({"format": "json"}));
    last_copy_is(&log, "json");
    control_run(&mut h, "file.exportPdf", json!({}));
    last_copy_is(&log, "pdf");
    let out = log.lock().unwrap();
    assert_eq!(out.copies.len(), SAVE_AS.formats().len() + 3 + 4 + 6 + 2, "every entry point delivered one copy");
    assert!(out.copies.iter().all(|(name, ..)| name.starts_with("Book1.")), "copies are named after the workbook");
    assert!(out.saves.is_empty(), "no Save As goes the plain Save way");
    drop(out);
    assert_eq!(doc_state(&h), before, "the workbook keeps its file, name and unsaved changes");
    assert!(h.state().message.is_none(), "{:?}", h.state().message);
}

#[test]
fn save_as_with_no_type_opens_the_save_as_page() {
    let log = Log::default();
    let mut h = harness(web(&log, Some(Runtime::Web)));
    let opened = |h: &mut Harness, how: &str, go: &dyn Fn(&mut Harness)| {
        h.state_mut().backstage = None;
        go(h);
        h.run_steps(2);
        assert_eq!(page(h), Some("saveAs"), "{how} opens the Save As page");
    };
    opened(&mut h, "F12", &|h| h.key_press(Key::F12));
    opened(&mut h, "Cmd/Ctrl+Shift+S", &|h| h.key_press_modifiers(Modifiers::COMMAND | Modifiers::SHIFT, Key::S));
    opened(&mut h, "the title bar's Save As…", &|h| {
        h.get_by_label("More commands").click();
        h.run_steps(3);
        h.get_by_label("Save As…").click();
    });
    opened(&mut h, "command search", &|h| command_search(h, "file.saveAs"));
    opened(&mut h, "the control channel", &|h| control_run(h, "file.saveAs", json!({})));
    assert!(log.lock().unwrap().copies.is_empty(), "nothing is delivered before a type is chosen");
    // A host without the File tab: Save As gives a workbook copy at once.
    h.state_mut().backstage = None;
    h.state_mut().chrome.file_tab = false;
    h.key_press(Key::F12);
    h.run_steps(2);
    last_copy_is(&log, "xlsx");
}

#[test]
fn framed_by_a_page_that_stores_saves_save_as_downloads_and_the_page_gets_only_plain_save() {
    let log = Log::default();
    let mut h = harness(web(&log, Some(Runtime::WebHosted)));
    let before = doc_state(&h);
    for f in SAVE_AS.formats() {
        save_as_page(&mut h, f.id, &format!("Download (.{})", f.ext));
        last_copy_is(&log, f.ext);
    }
    title_bar_export(&mut h, "Export as PDF…");
    last_copy_is(&log, "pdf");
    assert!(log.lock().unwrap().saves.is_empty(), "no Save As or export is handed to the page");
    assert_eq!(doc_state(&h), before);
    assert!(h.state().message.is_none(), "no refusal: Save As always gives a copy");
    // A shell that gives no copy download refuses rather than handing the copy to the page.
    h.state_mut().services.download_copy = None;
    assert!(h.state_mut().save_as_copy("csv").is_err());
    assert!(log.lock().unwrap().saves.is_empty());
    // Plain Save still goes to the page.
    h.state_mut().run_or_alert("file.save", json!({}));
    h.run_steps(2);
    assert_eq!(log.lock().unwrap().saves.len(), 1, "plain Save hands the workbook to the page");
}

#[test]
fn on_the_desktop_save_as_writes_a_copy_where_the_dialog_says() {
    let dir = std::env::temp_dir().join(format!("gridcraft-save-as-copy-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let asked = Arc::new(Mutex::new(Vec::<(String, String, String)>::new()));
    let pick = Arc::new(Mutex::new(dir.join("Copy").to_string_lossy().into_owned()));
    let (capture, answer) = (asked.clone(), pick.clone());
    let mut h = harness(Services {
        runtime: Some(Runtime::Desktop),
        pick_save: Some(Box::new(|_| panic!("Save As uses the typed dialog"))),
        pick_save_as: Some(Box::new(move |name, label, ext| {
            capture.lock().unwrap().push((name.into(), label.into(), ext.into()));
            Some(answer.lock().unwrap().clone())
        })),
        ..Default::default()
    });
    let book = dir.join("Book.xlsx").to_string_lossy().into_owned();
    h.state_mut().run("file.save", json!({"path": &book})).unwrap();
    h.state_mut().run("cell.set", json!({"cell": "A2", "input": "7"})).unwrap();
    let before = doc_state(&h);
    assert!(before.2, "the workbook has unsaved changes");

    save_as_page(&mut h, "xlsx", "Browse…");
    let (name, label, ext) = asked.lock().unwrap().last().cloned().unwrap();
    assert_eq!((name.as_str(), ext.as_str()), ("Book.xlsx", "xlsx"), "the dialog suggests the workbook's name and offers its type");
    assert!(label.contains("xlsx"));
    let copy = dir.join("Copy.xlsx");
    let (wb, _) = gridcraft_engine::io::open_bytes("Copy.xlsx", &std::fs::read(&copy).unwrap()).unwrap();
    assert_eq!(wb.active().unwrap().value(CellRef::new(1, 0)), Value::Number(7.0), "the copy has the unsaved change");
    assert_eq!(doc_state(&h), before, "the window stays on Book.xlsx, still unsaved");

    // The dialog confirmed "Copy", not "Copy.xlsx": an existing Copy.xlsx is not overwritten.
    let written = std::fs::read(&copy).unwrap();
    h.state_mut().run("cell.set", json!({"cell": "A3", "input": "8"})).unwrap();
    h.state_mut().save_as_copy_or_alert("xlsx");
    assert!(h.state().message.as_ref().is_some_and(|(_, m)| m.contains("already exists")), "{:?}", h.state().message);
    assert_eq!(std::fs::read(&copy).unwrap(), written, "the existing file is left alone");
    h.state_mut().message = None;
    // Picked by its full name, the dialog has asked about overwriting it.
    *pick.lock().unwrap() = copy.to_string_lossy().into_owned();
    assert!(h.state_mut().save_as_copy_or_alert("xlsx"));
    assert_ne!(std::fs::read(&copy).unwrap(), written);

    // Every other type, through the same dialog.
    for f in SAVE_AS.formats().iter().filter(|f| f.id != "xlsx") {
        *pick.lock().unwrap() = dir.join("Other").to_string_lossy().into_owned();
        assert!(h.state_mut().save_as_copy_or_alert(f.id), "{}: {:?}", f.id, h.state().message);
        assert!(dir.join(format!("Other.{}", f.ext)).exists(), "{} is written", f.ext);
    }
    assert_eq!(doc_state(&h).0, before.0, "the workbook's file never changes");
    assert!(doc_state(&h).2);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn file_save_of_a_new_workbook_is_a_plain_save_that_gives_it_its_file() {
    let dir = std::env::temp_dir().join(format!("gridcraft-save-new-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let target = dir.join("New.xlsx").to_string_lossy().into_owned();
    let answer = target.clone();
    let mut h = harness(Services { pick_save: Some(Box::new(move |_| Some(answer.clone()))), ..Default::default() });
    h.get_by_label("File").click();
    h.run_steps(3);
    click(&mut h, "Save");
    let (path, _, dirty) = doc_state(&h);
    assert_eq!(path.as_deref(), Some(target.as_str()), "Save makes the picked file the workbook's");
    assert!(!dirty);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Automation (`run`, the control channel) keeps each command's own behaviour: Export as CSV or
/// web page needs a `path` and honours its options; Export as PDF with no path returns the bytes.
#[test]
fn automation_keeps_the_commands_own_behaviour() {
    let log = Log::default();
    let mut h = harness(web(&log, Some(Runtime::Web)));
    for id in ["file.exportCsv", "file.exportHtml"] {
        let e = h.state_mut().run(id, json!({})).unwrap_err();
        assert!(e.contains("path"), "{id}: {e}");
    }
    let pdf = h.state_mut().run("file.exportPdf", json!({"range": "A1:B2"})).unwrap();
    assert!(pdf.get("base64").and_then(serde_json::Value::as_str).is_some_and(|b| !b.is_empty()), "the PDF bytes come back: {pdf}");
    last_copy_is(&log, "pdf");
    assert!(log.lock().unwrap().saves.is_empty());

    let dir = std::env::temp_dir().join(format!("gridcraft-automation-export-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut h = harness(Services { pick_save_as: Some(Box::new(|_, _, _| panic!("automation opens no dialog"))), ..Default::default() });
    h.state_mut().run("cell.set", json!({"cell": "A2", "input": "=1/3"})).unwrap();
    let csv = dir.join("values.csv");
    h.state_mut().run("file.exportCsv", json!({"path": &csv, "values": true})).unwrap();
    assert!(std::fs::read_to_string(&csv).unwrap().contains("0.333333"), "`values: true` writes full precision");
    let desktop_pdf = h.state_mut().run("file.exportPdf", json!({})).unwrap();
    assert!(desktop_pdf.get("base64").is_some(), "desktop automation gets the PDF bytes, with no dialog");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The desktop copy is encoded for the file it goes to: a relative link to another workbook still
/// names the same file from the copy's folder, as plain Save to another folder does. The workbook
/// keeps its own file and link.
#[test]
fn a_desktop_copy_in_another_folder_keeps_relative_links_on_the_same_file() {
    let a = std::env::temp_dir().join(format!("gridcraft-copy-link-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&a);
    let b = a.join("b");
    std::fs::create_dir_all(&b).unwrap();
    let dest = a.join("Dest.xlsx");
    std::fs::write(&dest, gridcraft_xlsx::testing::linked_workbook("Source.xlsx")).unwrap();
    let target = b.join("Copy.xlsx").to_string_lossy().into_owned();
    let mut h = crate::support::services_harness(
        Services { runtime: Some(Runtime::Desktop), pick_save_as: Some(Box::new(move |_, _, _| Some(target.clone()))), ..Default::default() },
        4,
    );
    h.state_mut().run("file.open", json!({"path": &dest})).unwrap();
    h.run_steps(2);
    assert!(h.state_mut().save_as_copy_or_alert("xlsx"), "{:?}", h.state().message);
    assert_eq!(gridcraft_xlsx::testing::link_target(&std::fs::read(b.join("Copy.xlsx")).unwrap()), "../Source.xlsx");
    assert_eq!(h.state().session.active().unwrap().path.as_deref(), Some(dest.to_str().unwrap()), "the window stays on Dest.xlsx");
    let _ = std::fs::remove_dir_all(&a);
}

/// Desktop entry points other than the Save As page: the title bar's Export menu, the menu bar's
/// Export as CSV and Export as PDF (with its options), F12 without the File tab, and a
/// cancelled dialog, which writes nothing and says nothing.
#[test]
fn desktop_menus_and_keys_save_copies_through_the_typed_dialog() {
    let dir = std::env::temp_dir().join(format!("gridcraft-desktop-entry-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let asked = Arc::new(Mutex::new(Vec::<String>::new()));
    let answer = Arc::new(Mutex::new(Some(dir.join("Out").to_string_lossy().into_owned())));
    let (capture, pick) = (asked.clone(), answer.clone());
    let mut h = harness(Services {
        runtime: Some(Runtime::Desktop),
        pick_save_as: Some(Box::new(move |_, _, ext| {
            capture.lock().unwrap().push(ext.into());
            pick.lock().unwrap().clone()
        })),
        ..Default::default()
    });
    let before = doc_state(&h);
    title_bar_export(&mut h, "Export as Web Page (.html)…");
    assert!(dir.join("Out.html").exists());
    h.state_mut().run_or_alert("file.exportCsv", json!({}));
    assert!(dir.join("Out.csv").exists());
    h.state_mut().run_or_alert("file.exportPdf", json!({"range": "A1:A1"}));
    assert!(std::fs::read(dir.join("Out.pdf")).unwrap().starts_with(b"%PDF-"));
    h.state_mut().chrome.file_tab = false;
    h.key_press(Key::F12);
    h.run_steps(2);
    assert!(dir.join("Out.xlsx").exists(), "F12 without the File tab saves a workbook copy");
    assert_eq!(*asked.lock().unwrap(), ["html", "csv", "pdf", "xlsx"], "each dialog offers the chosen type");
    *answer.lock().unwrap() = None;
    assert!(!h.state_mut().save_as_copy_or_alert("tsv"));
    assert!(!dir.join("Out.tsv").exists() && h.state().message.is_none(), "a cancelled dialog writes nothing and says nothing");
    assert_eq!(doc_state(&h), before);
    let _ = std::fs::remove_dir_all(&dir);
}

/// File › Save of a new workbook is plain Save, as it always was: on the desktop the picked file
/// becomes the workbook's (a cancelled pick keeps the File view open); on the web it downloads, or
/// goes to a page that stores saves. Save As copies are not involved.
#[test]
fn file_save_of_a_new_workbook_on_the_web_and_hosted_is_the_plain_save() {
    for runtime in [Runtime::Web, Runtime::WebHosted] {
        let log = Log::default();
        let mut h = harness(web(&log, Some(runtime)));
        h.get_by_label("File").click();
        h.run_steps(3);
        click(&mut h, "Save");
        let out = log.lock().unwrap();
        assert_eq!(out.saves.len(), 1, "{runtime:?}: the workbook goes the plain-Save way");
        assert!(out.saves[0].ends_with(".xlsx"));
        assert!(out.copies.is_empty(), "{runtime:?}: no Save As copy");
        drop(out);
        assert_eq!(page(&h), None);
    }
    let mut h = harness(Services { pick_save: Some(Box::new(|_| None)), ..Default::default() });
    h.get_by_label("File").click();
    h.run_steps(3);
    click(&mut h, "Save");
    assert!(h.state().backstage.is_some(), "a cancelled pick keeps the File view");
    assert!(doc_state(&h).0.is_none());
}
