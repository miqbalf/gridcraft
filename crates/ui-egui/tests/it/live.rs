//! The live co-editing interface: another editor's selection, the command gate, the status line,
//! and the cell editor that stays on its row while rows move under it.
//!
//! Alice is the window under test; Bob is a plain session in the same in-memory room whom the
//! test drives by hand.

use std::sync::Arc;

use craft_collab::{ClientMsg, Hub};
use craft_live::Access;
use craft_live::testing::{MemRelay, ROOM};
use egui::{Color32, Context, RawInput, Rect, Shape, pos2, vec2};
use egui_kittest::kittest::{NodeT as _, Queryable as _};
use gridcraft_engine::core::{CellRef, Value};
use gridcraft_engine::model::{Cell, Sheet, Workbook};
use gridcraft_engine::{Session, io};
use gridcraft_live::{BaseFile, GridLive};
use gridcraft_ui_egui::live::DynHub;
use gridcraft_ui_egui::{Services, SheetApp, theme};
use serde_json::json;

const FILE: &str = "book.xlsx";

fn a1(s: &str) -> CellRef {
    gridcraft_engine::core::RangeRef::parse(s).map(|r| r.start).expect("a cell")
}

/// Two sheets; column A holds 1..8 on the first.
fn base_bytes() -> Vec<u8> {
    let mut wb = Workbook::new();
    if wb.sheets.len() < 2 {
        let name = wb.next_sheet_name();
        wb.sheets.push(Arc::new(Sheet::new(name)));
    }
    if let Some(s) = wb.sheet_mut(0) {
        for r in 0..8u32 {
            s.set_cell(CellRef::new(r, 0), Cell::value(Value::Number(f64::from(r + 1))));
        }
    }
    io::save_bytes(&wb, FILE).expect("base saves")
}

/// The other editor: an engine session on a plain in-memory hub.
struct Bob {
    session: Session,
    live: GridLive<craft_live::testing::MemHub>,
}

impl Bob {
    fn pump(&mut self) {
        self.live.pump(&mut self.session);
        self.live.send_selection(&self.session);
    }

    fn run(&mut self, id: &str, params: serde_json::Value) {
        self.live.run(&mut self.session, id, params).expect("bob's command runs");
    }

    fn select(&mut self, range: &str, active: &str) {
        self.session.run("selection.set", json!({"range": range, "active": active})).expect("selects");
    }
}

/// Alice's hub, noting the rooms she told the relay she left.
struct Spy {
    inner: craft_live::testing::MemHub,
    left: Arc<std::sync::Mutex<Vec<String>>>,
}

impl Hub for Spy {
    fn send(&mut self, msg: ClientMsg) -> craft_collab::Result<()> {
        if let ClientMsg::Leave { doc } = &msg {
            self.left.lock().expect("lock").push(doc.clone());
        }
        self.inner.send(msg)
    }

    fn poll(&mut self) -> Vec<craft_collab::ServerMsg> {
        self.inner.poll()
    }

    fn connected(&self) -> bool {
        self.inner.connected()
    }
}

struct Rig {
    left: Arc<std::sync::Mutex<Vec<String>>>,
    relay: MemRelay,
    bytes: Vec<u8>,
    bob: Bob,
}

fn join(relay: &MemRelay, bytes: &[u8], uid: &str, name: &str, access: Access) -> (Session, GridLive<craft_live::testing::MemHub>) {
    let hub = relay.connect(uid, name, access == Access::Edit);
    let mut session = Session::new();
    let base = BaseFile { bytes, name: FILE, expected_sha256: None };
    let live = GridLive::start(hub, ROOM, &base, access, uid, &mut session).expect("starts");
    (session, live)
}

/// Alice's window, joined to the room; Bob is already in it.
fn rig(alice_access: Access) -> (Rig, SheetApp) {
    let relay = MemRelay::new();
    let bytes = base_bytes();
    let (session, live) = join(&relay, &bytes, "bob", "Bob", Access::Edit);
    let mut t = 0.0;
    let mut bob = Bob { session, live };
    bob.live.set_clock(Box::new(move || {
        t += 1000.0;
        t
    }));
    // Alice joins through a boxed hub, as a host picking its transport at run time does.
    let hub = relay.connect("alice", "Alice", alice_access == Access::Edit);
    let mut session = Session::new();
    let base = BaseFile { bytes: &bytes, name: FILE, expected_sha256: None };
    let left = Arc::new(std::sync::Mutex::new(Vec::new()));
    let hub = Spy { inner: hub, left: Arc::clone(&left) };
    let live = GridLive::start(DynHub(Box::new(hub)), ROOM, &base, alice_access, "alice", &mut session).expect("starts");
    let app = crate::common::prepare(SheetApp::new(session, Services { live: Some(live), ..Default::default() }));
    (Rig { left, relay, bytes, bob }, app)
}

type Harness = crate::common::Harness;

fn harness(app: SheetApp) -> Harness {
    crate::common::harness_with(crate::common::builder().with_size(vec2(1200.0, 800.0)), app, 3)
}

/// Lets both windows take in everything in flight.
fn settle(rig: &mut Rig, h: &mut Harness) {
    let mut quiet = 0;
    for _ in 0..80 {
        rig.bob.pump();
        h.step();
        if rig.relay.quiet() {
            quiet += 1;
            if quiet >= 3 {
                return;
            }
        } else {
            quiet = 0;
        }
    }
}

fn value(app: &SheetApp, sheet: usize, cell: &str) -> Value {
    app.session.active().and_then(|d| d.wb.sheet(sheet)).and_then(|s| s.cell(a1(cell))).map(|c| c.value.clone()).unwrap_or(Value::Empty)
}

fn bob_value(rig: &Rig, cell: &str) -> Value {
    rig.bob.session.active().and_then(|d| d.wb.sheet(0)).and_then(|s| s.cell(a1(cell))).map(|c| c.value.clone()).unwrap_or(Value::Empty)
}

fn notice_text(app: &SheetApp) -> String {
    app.notice.as_ref().map(|n| n.text.clone()).unwrap_or_default()
}

// --- presence drawing ---------------------------------------------------------------------

#[test]
fn remote_selection_is_drawn_with_a_named_accessibility_node() {
    let (mut rig, app) = rig(Access::Edit);
    let mut h = harness(app);
    rig.bob.select("B3:D9", "B3");
    settle(&mut rig, &mut h);
    h.get_by_label("Bob's selection, B3:D9 on Sheet1");
}

fn frame(ctx: &Context, app: &mut SheetApp) -> egui::FullOutput {
    let mut output =
        ctx.run_ui(RawInput { screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(1200.0, 800.0))), ..Default::default() }, |ui| {
            let ctx = ui.ctx().clone();
            app.logic(&ctx);
            app.ui(ui);
        });
    output.textures_delta.clear();
    output
}

/// Runs plain frames (no harness), pumping Bob between them.
fn frames(rig: &mut Rig, ctx: &Context, app: &mut SheetApp) -> egui::FullOutput {
    let mut last = frame(ctx, app);
    for _ in 0..40 {
        rig.bob.pump();
        last = frame(ctx, app);
    }
    last
}

#[test]
fn badge_and_outline_share_a_colour() {
    let (mut rig, mut app) = rig(Access::Edit);
    let carol = {
        let (session, live) = join(&rig.relay, &rig.bytes, "carol", "Carol", Access::Edit);
        Bob { session, live }
    };
    let mut carol = carol;
    let ctx = Context::default();
    theme::apply(&ctx, false);
    rig.bob.select("B3", "B3");
    carol.select("D5", "D5");
    let mut out = frame(&ctx, &mut app);
    for _ in 0..40 {
        rig.bob.pump();
        carol.pump();
        out = frame(&ctx, &mut app);
    }
    let mut outlines: Vec<Color32> = Vec::new();
    let mut badges: Vec<Color32> = Vec::new();
    for clipped in &out.shapes {
        if let Shape::Rect(r) = &clipped.shape {
            if r.fill == Color32::TRANSPARENT && r.stroke.width == 2.0 {
                outlines.push(r.stroke.color);
            }
            if r.fill != Color32::TRANSPARENT && r.rect.width() == 24.0 && r.rect.height() == 24.0 {
                badges.push(r.fill);
            }
        }
    }
    let live = app.live().expect("live");
    assert_eq!(live.peers().len(), 2, "both other editors are here");
    for colour in live.colours() {
        assert!(outlines.contains(&colour), "an outline is drawn in {colour:?}: {outlines:?}");
        assert!(badges.contains(&colour), "a badge is drawn in {colour:?}: {badges:?}");
    }
}

#[test]
fn peer_on_another_sheet_shows_a_tab_dot() {
    let (mut rig, mut app) = rig(Access::Edit);
    let ctx = Context::default();
    theme::apply(&ctx, false);
    let dots =
        |out: &egui::FullOutput, colour: Color32| out.shapes.iter().filter(|s| matches!(&s.shape, Shape::Circle(c) if c.fill == colour)).count();
    frames(&mut rig, &ctx, &mut app);
    rig.bob.select("B3", "B3");
    let out = frames(&mut rig, &ctx, &mut app);
    let colour = app.live().expect("live").colours()[0];
    let same_sheet = dots(&out, colour);
    rig.bob.session.run("sheet.activate", json!({"sheet": 1})).expect("sheet");
    rig.bob.select("C2", "C2");
    let out = frames(&mut rig, &ctx, &mut app);
    assert!(dots(&out, colour) > same_sheet, "a dot on the other sheet's tab ({} vs {same_sheet})", dots(&out, colour));
}

#[test]
fn clicking_a_badge_jumps_to_the_peer_cell() {
    let (mut rig, app) = rig(Access::Edit);
    let mut h = harness(app);
    rig.bob.select("D9", "D9");
    settle(&mut rig, &mut h);
    assert_ne!(h.state().session.active().expect("doc").selection.active, a1("D9"));
    h.get_by_label("Bob").click();
    h.run_steps(3);
    assert_eq!(h.state().session.active().expect("doc").selection.active, a1("D9"));
}

#[test]
fn viewer_is_labelled_viewing() {
    // Bob only reads: Alice's presence bar says so (the roster has no access, so the host says).
    let (mut rig, app) = rig(Access::Edit);
    let mut h = harness(app);
    settle(&mut rig, &mut h);
    let id = h.state().live().expect("live").peers()[0].id.clone();
    h.state_mut().live_mut().expect("live").set_viewers([id]);
    settle(&mut rig, &mut h);
    h.get_by_label("Bob (viewing)");
    // And a window that only reads says so about itself.
    let (mut rig, app) = rig_viewer();
    let mut h = harness(app);
    settle(&mut rig, &mut h);
    h.get_by_label("Viewing only");
}

fn rig_viewer() -> (Rig, SheetApp) {
    rig(Access::Read)
}

// --- the command gate and the status line ---------------------------------------------------

#[test]
fn gated_command_is_greyed_with_the_reason() {
    let (mut rig, app) = rig(Access::Edit);
    let mut h = harness(app);
    h.state_mut().ui.ribbon_tab = "Data".into();
    settle(&mut rig, &mut h);
    assert!(h.state().live_gates("data.removeDuplicates"), "the gate refuses it");
    let button = h.get_by_label("Remove Duplicates");
    assert!(button.accesskit_node().is_disabled(), "greyed");
    // A command the session shares stays enabled.
    h.state_mut().ui.ribbon_tab = "Home".into();
    settle(&mut rig, &mut h);
    assert!(!h.get_by_label("Paste").accesskit_node().is_disabled());
    // The reason shows as the tooltip of the greyed button.
    h.state_mut().ui.ribbon_tab = "Data".into();
    settle(&mut rig, &mut h);
    h.get_by_label("Remove Duplicates").hover();
    for _ in 0..6 {
        h.step();
    }
    h.get_by_label_contains(gridcraft_live::NOT_YET_TEXT);
    // The refusal says why.
    let err = h.state_mut().run("data.removeDuplicates", json!({})).expect_err("refused");
    assert!(err.contains(gridcraft_live::NOT_YET_TEXT), "{err}");
}

#[test]
fn read_only_blocks_typing_and_says_so() {
    let (mut rig, app) = rig(Access::Read);
    let mut h = harness(app);
    settle(&mut rig, &mut h);
    h.state_mut().begin_edit(Some("typed".into()), false);
    assert!(h.state().editor.is_none(), "no entry is opened");
    let said = h.state().message.as_ref().map(|(_, m)| m.clone()).unwrap_or_default();
    assert!(said.to_lowercase().contains("read"), "{said}");
    let err = h.state_mut().run("cell.set", json!({"cell": "B2", "input": "9"})).expect_err("refused");
    assert!(err.to_lowercase().contains("read"), "{err}");
    assert_eq!(value(h.state(), 0, "B2"), Value::Empty);
}

#[test]
fn status_line_names_the_lock_holder_while_waiting() {
    let (mut rig, app) = rig(Access::Edit);
    let mut h = harness(app);
    settle(&mut rig, &mut h);
    // A third window holds the structure lock and never lets go.
    let mut holder = rig.relay.connect("dan", "Dan", true);
    holder.send(ClientMsg::Lock { doc: ROOM.into(), scope: "grid/struct".into() }).expect("sends");
    settle(&mut rig, &mut h);
    h.state_mut().run("home.insertRows", json!({"rows": "2:2"})).expect("queued behind the lock");
    settle(&mut rig, &mut h);
    h.get_by_label_contains("Waiting for Dan");
}

// --- the cell editor stays on its row ---------------------------------------------------------

fn start_editing(h: &mut Harness, cell: &str, text: &str) {
    h.state_mut().run("selection.set", json!({"cell": cell})).expect("selects");
    h.state_mut().begin_edit(Some(text.into()), false);
    assert!(h.state().editor.is_some(), "the editor opened");
}

#[test]
fn open_editor_keeps_its_draft_through_a_remote_change_elsewhere() {
    let (mut rig, app) = rig(Access::Edit);
    let mut h = harness(app);
    settle(&mut rig, &mut h);
    start_editing(&mut h, "B6", "hello");
    rig.bob.run("cell.set", json!({"cell": "C2", "input": "7"}));
    settle(&mut rig, &mut h);
    let ed = h.state().editor.as_ref().expect("still open");
    assert_eq!((ed.cell, ed.text.as_str()), (a1("B6"), "hello"));
    assert!(h.state_mut().commit_edit(0, 0, false, false));
    settle(&mut rig, &mut h);
    assert_eq!(value(h.state(), 0, "B6"), Value::Text("hello".into()));
    assert_eq!(bob_value(&rig, "B6"), Value::Text("hello".into()));
}

#[test]
fn open_editor_follows_its_row_through_a_remote_insert() {
    // The answer to the spec's open question: the editor moves with its row, and Enter commits
    // to the moved cell (as Google Sheets does), not to the cell now at the old position.
    let (mut rig, app) = rig(Access::Edit);
    let mut h = harness(app);
    settle(&mut rig, &mut h);
    start_editing(&mut h, "B6", "hello");
    rig.bob.run("home.insertRows", json!({"rows": "2:2"}));
    settle(&mut rig, &mut h);
    assert_eq!(h.state().editor.as_ref().expect("still open").cell, a1("B7"), "the editor moved down with its row");
    assert!(h.state_mut().commit_edit(0, 0, false, false));
    settle(&mut rig, &mut h);
    for (what, v) in [("alice", value(h.state(), 0, "B7")), ("bob", bob_value(&rig, "B7"))] {
        assert_eq!(v, Value::Text("hello".into()), "{what}: committed to the moved cell");
    }
    assert_eq!(value(h.state(), 0, "B6"), Value::Empty, "nothing landed in the old position");
}

#[test]
fn open_editor_follows_its_row_through_a_remote_delete_above() {
    let (mut rig, app) = rig(Access::Edit);
    let mut h = harness(app);
    settle(&mut rig, &mut h);
    start_editing(&mut h, "B6", "hello");
    rig.bob.run("home.deleteRows", json!({"rows": "2:3"}));
    settle(&mut rig, &mut h);
    assert_eq!(h.state().editor.as_ref().expect("still open").cell, a1("B4"), "the editor moved up with its row");
    assert!(h.state_mut().commit_edit(0, 0, false, false));
    settle(&mut rig, &mut h);
    assert_eq!(value(h.state(), 0, "B4"), Value::Text("hello".into()));
    assert_eq!(bob_value(&rig, "B4"), Value::Text("hello".into()));
}

#[test]
fn deleting_the_editors_own_row_closes_it_and_keeps_the_text_in_a_notice() {
    let (mut rig, app) = rig(Access::Edit);
    let mut h = harness(app);
    settle(&mut rig, &mut h);
    start_editing(&mut h, "B6", "my precious entry");
    rig.bob.run("home.deleteRows", json!({"rows": "6:6"}));
    settle(&mut rig, &mut h);
    assert!(h.state().editor.is_none(), "the editor closed");
    let told = notice_text(h.state());
    assert!(told.contains("my precious entry"), "the typed text is not lost: {told:?}");
    assert_eq!(value(h.state(), 0, "B6"), Value::Empty, "it was not written into the neighbour that took the row's place");
}

#[test]
fn a_formula_in_an_open_editor_is_read_at_commit_against_the_current_grid() {
    let (mut rig, app) = rig(Access::Edit);
    let mut h = harness(app);
    settle(&mut rig, &mut h);
    start_editing(&mut h, "B6", "=A8");
    rig.bob.run("home.insertRows", json!({"rows": "2:2"}));
    settle(&mut rig, &mut h);
    assert!(h.state_mut().commit_edit(0, 0, false, false));
    settle(&mut rig, &mut h);
    // A8 is now the old A7 (7), because a row went in above it: the text was read when entered.
    assert_eq!(value(h.state(), 0, "B7"), Value::Number(7.0));
    assert_eq!(bob_value(&rig, "B7"), Value::Number(7.0));
}

#[test]
fn a_session_attached_after_construction_edits_live() {
    let relay = MemRelay::new();
    let bytes = base_bytes();
    let (session, live) = join(&relay, &bytes, "bob", "Bob", Access::Edit);
    let bob = Bob { session, live };
    let hub = relay.connect("alice", "Alice", true);
    let mut session = Session::new();
    let base = BaseFile { bytes: &bytes, name: FILE, expected_sha256: None };
    let live = GridLive::start(DynHub(Box::new(hub)), ROOM, &base, Access::Edit, "alice", &mut session).expect("starts");
    // The window is built first, with no session, then the host attaches it.
    let mut app = crate::common::prepare(SheetApp::new(session, Services::default()));
    assert!(app.live().is_none());
    app.attach_live(live).expect("attaches");
    let second = {
        let hub = relay.connect("zed", "Zed", true);
        let mut s = Session::new();
        GridLive::start(DynHub(Box::new(hub)), ROOM, &base, Access::Edit, "zed", &mut s).expect("starts")
    };
    assert!(app.attach_live(second).is_err(), "a second session is refused");
    let mut h = harness(app);
    let mut rig = Rig { left: Arc::default(), relay, bytes, bob };
    settle(&mut rig, &mut h);
    h.state_mut().run("cell.set", json!({"cell": "B2", "input": "5"})).expect("runs through the session");
    settle(&mut rig, &mut h);
    assert_eq!(bob_value(&rig, "B2"), Value::Number(5.0), "the change reached the other editor");
}

#[test]
fn a_dropdown_whose_label_clashes_with_a_refused_command_stays_enabled() {
    // "Clear" is also the label of `data.clearFilter` (refused), but Home's Clear menu holds
    // commands the session shares.
    let (mut rig, app) = rig(Access::Edit);
    let mut h = harness(app);
    settle(&mut rig, &mut h);
    assert!(!h.get_by_label("Clear").accesskit_node().is_disabled());
}

#[test]
fn typing_in_another_workbook_is_not_written_into_the_live_one() {
    let (mut rig, app) = rig(Access::Edit);
    let mut h = harness(app);
    settle(&mut rig, &mut h);
    let live_doc = h.state().session.active().expect("doc").uid;
    h.state_mut().run("file.new", json!({})).expect("a second workbook opens");
    assert_ne!(h.state().session.active().expect("doc").uid, live_doc, "the new workbook is active");
    h.state_mut().run("selection.set", json!({"cell": "C3"})).expect("selects");
    h.state_mut().begin_edit(Some("private".into()), false);
    assert!(h.state_mut().commit_edit(0, 0, false, false));
    settle(&mut rig, &mut h);
    let active = h.state().session.active().expect("doc");
    assert_ne!(active.uid, live_doc, "still in the second workbook");
    assert_eq!(
        active.wb.sheet(0).and_then(|s| s.cell(a1("C3"))).map(|c| c.value.clone()),
        Some(Value::Text("private".into())),
        "it went into its own workbook"
    );
    assert_eq!(bob_value(&rig, "C3"), Value::Empty, "nothing reached the other editor");
    let live = h.state().session.documents().iter().find(|d| d.uid == live_doc).expect("live doc");
    assert_eq!(live.wb.sheet(0).and_then(|s| s.cell(a1("C3"))).map(|c| c.value.clone()), None, "and the live workbook is untouched");
}

#[test]
fn closing_the_live_workbook_ends_the_session_and_the_window_stays_usable() {
    let (mut rig, app) = rig(Access::Edit);
    let mut h = harness(app);
    settle(&mut rig, &mut h);
    h.state_mut().run("file.close", json!({"force": true})).expect("closes");
    settle(&mut rig, &mut h);
    assert!(h.state().live().is_none(), "the session ended with its workbook");
    assert!(notice_text(h.state()).to_lowercase().contains("live editing ended"), "{:?}", notice_text(h.state()));
    assert_eq!(h.state().session.live_undo_doc(), None, "the engine's hooks came off");
    h.state_mut().run("cell.set", json!({"cell": "A1", "input": "x"})).expect("the window still edits");
}

#[test]
fn a_deleted_row_draft_is_kept_whatever_else_the_room_says() {
    let (mut rig, app) = rig(Access::Edit);
    let mut h = harness(app);
    settle(&mut rig, &mut h);
    start_editing(&mut h, "B6", "short draft");
    rig.bob.run("home.deleteRows", json!({"rows": "6:6"}));
    // Another notice arrives in the same and in later frames.
    h.state_mut().live_mut().expect("live").live_mut().core_mut().push_notice("Something else happened".into());
    settle(&mut rig, &mut h);
    h.state_mut().live_mut().expect("live").live_mut().core_mut().push_notice("And again".into());
    settle(&mut rig, &mut h);
    let said = h.state().message.as_ref().map(|(_, m)| m.clone()).unwrap_or_default();
    assert!(said.contains("short draft"), "the entry stays in the message box until it is closed: {said:?}");
}

#[test]
fn notices_from_one_pump_are_all_shown_one_after_the_other() {
    let (mut rig, app) = rig(Access::Edit);
    let mut h = harness(app);
    settle(&mut rig, &mut h);
    let live = h.state_mut().live_mut().expect("live").live_mut();
    live.core_mut().push_notice("First thing".into());
    live.core_mut().push_notice("Second thing".into());
    settle(&mut rig, &mut h);
    assert!(notice_text(h.state()).contains("First thing"), "{:?}", notice_text(h.state()));
    h.state_mut().notice = None;
    settle(&mut rig, &mut h);
    assert!(notice_text(h.state()).contains("Second thing"), "{:?}", notice_text(h.state()));
}

#[test]
fn a_session_started_on_another_engine_session_is_refused() {
    let relay = MemRelay::new();
    let bytes = base_bytes();
    // Started on a scratch session, then handed to a window that has another one.
    let hub = relay.connect("alice", "Alice", true);
    let mut scratch = Session::new();
    let base = BaseFile { bytes: &bytes, name: FILE, expected_sha256: None };
    let live = GridLive::start(DynHub(Box::new(hub)), ROOM, &base, Access::Edit, "alice", &mut scratch).expect("starts");
    let mut own = Session::new();
    own.new_workbook();
    let mut app = crate::common::prepare(SheetApp::new(own, Services::default()));
    assert!(app.attach_live(live).is_err(), "no edit hook on this window's session");
    assert!(app.live().is_none());
    // The same through the constructor: refused with a message, not dropped silently.
    let hub = relay.connect("alice2", "Alice", true);
    let mut scratch = Session::new();
    let live = GridLive::start(DynHub(Box::new(hub)), ROOM, &base, Access::Edit, "alice2", &mut scratch).expect("starts");
    let mut own = Session::new();
    own.new_workbook();
    let app = crate::common::prepare(SheetApp::new(own, Services { live: Some(live), ..Default::default() }));
    assert!(app.live().is_none());
    assert!(app.message.as_ref().is_some_and(|(_, m)| m.contains("not started")), "{:?}", app.message);
}

/// Hovers the control and returns whether the refusal reason shows as its tooltip.
fn hover_shows_reason(h: &mut Harness, label: &str) -> bool {
    h.get_by_label(label).hover();
    for _ in 0..6 {
        h.step();
    }
    h.query_by_label_contains(gridcraft_live::NOT_YET_TEXT).is_some()
}

#[test]
fn a_refused_split_button_and_gallery_are_greyed_with_the_reason() {
    let (mut rig, app) = rig(Access::Edit);
    let mut h = harness(app);
    settle(&mut rig, &mut h);
    // The main part of the Merge & Center split button, gated by its command id.
    assert!(h.state().live_gates("home.mergeCenter"), "the gate refuses it");
    let merge = h.get_all_by_label("Merge & Center").next().expect("the control exists");
    assert!(merge.accesskit_node().is_disabled(), "the split button is greyed");
    assert!(hover_shows_reason(&mut h, "Merge & Center"), "its reason shows on hover");
    // A gallery whose items carry no command of their own.
    h.state_mut().ui.ribbon_tab = "Insert".into();
    settle(&mut rig, &mut h);
    assert!(h.state().live_gates("insert.shape"));
    assert!(h.get_by_label("Shapes").accesskit_node().is_disabled(), "the Shapes gallery is greyed");
    assert!(hover_shows_reason(&mut h, "Shapes"), "its reason shows on hover");
    // A control the session shares stays enabled.
    h.state_mut().ui.ribbon_tab = "Home".into();
    settle(&mut rig, &mut h);
    assert!(!h.get_by_label("Paste").accesskit_node().is_disabled());
}

#[test]
fn the_notice_queue_is_capped_and_its_count_stays_true() {
    let (mut rig, app) = rig(Access::Edit);
    let mut h = harness(app);
    settle(&mut rig, &mut h);
    let live = h.state_mut().live_mut().expect("live").live_mut();
    for i in 1..=30 {
        live.core_mut().push_notice(format!("Note number {i}."));
    }
    settle(&mut rig, &mut h);
    // 20 are kept, the oldest 10 went; one is on show and 19 wait.
    let shown = notice_text(h.state());
    assert!(shown.contains("Note number 11."), "{shown:?}");
    assert!(shown.contains("+19 more"), "{shown:?}");
    // Dismissing shows the next, and the count goes down with it.
    h.state_mut().notice = None;
    settle(&mut rig, &mut h);
    let shown = notice_text(h.state());
    assert!(shown.contains("Note number 12.") && shown.contains("+18 more"), "{shown:?}");
}

#[test]
fn ending_live_editing_leaves_the_room_by_name() {
    let (mut rig, app) = rig(Access::Edit);
    let mut h = harness(app);
    settle(&mut rig, &mut h);
    assert!(rig.left.lock().expect("lock").is_empty(), "still in the room");
    h.state_mut().run("file.close", json!({"force": true})).expect("closes");
    settle(&mut rig, &mut h);
    assert_eq!(*rig.left.lock().expect("lock"), vec![ROOM.to_owned()], "Alice told the relay she left, once");
}
