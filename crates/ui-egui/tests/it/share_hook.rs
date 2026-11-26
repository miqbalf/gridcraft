//! SH2 regression guard: on the live release line, Share is Nextcloud sharing (the host-share hook),
//! not an export menu. Uses only the app's public command and UI surface, so it also runs against
//! a build that lost the hook (where it fails).

use egui_kittest::kittest::Queryable as _;
use gridcraft_ui_egui::Services;
use serde_json::json;

const NO_NEXTCLOUD: &str = "No Nextcloud connected";

fn toast(h: &egui_kittest::Harness<'static, gridcraft_ui_egui::SheetApp>) -> Option<String> {
    h.state().toast.as_ref().map(|(t, _)| t.clone())
}

#[test]
fn file_share_is_a_command_that_never_exports() {
    let mut h = crate::support::services_harness(Services::default(), 4);
    let r = h.state_mut().run("file.share", json!({}));
    assert!(r.is_ok(), "file.share is a command: {r:?}");
    h.run_steps(2);
    assert_eq!(toast(&h).as_deref(), Some(NO_NEXTCLOUD), "no host and no desktop client: Share says so");
    assert!(h.state().dialog.is_none(), "Share opens no save-a-copy dialog");
}

#[test]
fn the_title_bar_share_opens_no_export_menu() {
    let mut h = crate::support::services_harness(Services::default(), 4);
    h.get_by_role_and_label(egui::accesskit::Role::Button, "Share").click();
    h.run_steps(3);
    assert!(h.query_by_label("Export as PDF…").is_none(), "the title bar's Share is not an export menu");
    assert_eq!(toast(&h).as_deref(), Some(NO_NEXTCLOUD), "the title bar's Share runs file.share");
}
