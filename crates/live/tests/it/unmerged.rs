//! The unmerged copy (G16 final): every path on which craft-live drops local edits the room does
//! not have hands the host a copy of the workbook with them, and a session the host ends or stops
//! keeps the edits the room never confirmed.

use chrono::NaiveDateTime;
use craft_collab::{ClientMsg, Hub, Replica, ServerMsg, TextPayload};
use craft_live::LiveStatus;
use craft_live::testing::ROOM;
use gridcraft_core::Value;
use gridcraft_live::{CellContent, GridDoc, Id, UNMERGED_TEXT, cell_key, unmerged_name};
use serde_json::json;

use crate::support::{FILE, Room, a1};

/// The value at `cell` of the first sheet of the copy's workbook.
fn copy_value(copy: &(String, Vec<u8>), cell: &str) -> Value {
    let (wb, _) = gridcraft_engine::io::open_bytes(&copy.0, &copy.1).expect("the copy opens");
    wb.sheet(0).and_then(|s| s.cell(a1(cell))).map(|c| c.value.clone()).unwrap_or(Value::Empty)
}

fn told(room: &Room, i: usize) -> bool {
    room.notices_of(i).iter().any(|n| n == UNMERGED_TEXT)
}

fn mem(room: &Room, i: usize) -> craft_live::testing::MemHub {
    room.peers[i].live.core().hub().inner().mem().clone()
}

/// Peer 0 types `text` into E5 and the relay keeps refusing it for now: an edit the room lacks.
fn type_unconfirmed(room: &mut Room, text: &str) {
    room.relay.refuse_next_submits(usize::MAX);
    room.run(0, "cell.set", json!({"cell": "E5", "input": text})).unwrap();
    room.pump(0);
    assert!(room.peers[0].live.core().live().unconfirmed_edits() > 0, "the edit waits for the room");
}

#[test]
fn unmerged_names_keep_the_stem() {
    let at = NaiveDateTime::parse_from_str("2026-11-24 1404", "%Y-%m-%d %H%M").unwrap();
    assert_eq!(unmerged_name("Book.xlsx", at), "Book (unmerged 2026-11-24 1404).xlsx");
    assert_eq!(unmerged_name("dir/Q3 plan.v2.xlsx", at), "Q3 plan.v2 (unmerged 2026-11-24 1404).xlsx");
}

/// Take-back: an ordered run made under a lease that lapsed is refused and taken back out of the
/// replica. The copy holds the run.
#[test]
fn a_taken_back_run_is_offered_as_the_unmerged_copy() {
    let mut room = Room::new(2);
    let base_a3 = room.base().sheet(0).unwrap().cell(a1("A3")).unwrap().value.clone();
    room.run(0, "home.insertRows", json!({"rows": "3:3"})).unwrap();
    room.relay.lapse("grid/struct");
    let mut bob = room.relay.connect("bob-raw", "Bob", true);
    bob.send(ClientMsg::Lock { doc: ROOM.into(), scope: "grid/struct".into() }).unwrap();
    room.settle();
    assert!(told(&room, 0), "told a copy waits: {:?}", room.notices_of(0));
    let copy = room.peers[0].live.take_unmerged().expect("the copy");
    assert_eq!(copy_value(&copy, "A4"), base_a3, "the copy holds the inserted row");
    assert_eq!(copy_value(&copy, "A3"), Value::Empty);
    assert!(room.peers[0].live.take_unmerged().is_none(), "taken once");
}

/// Replaced: the relay gives up catching the window up and sends the room's snapshot, which
/// replaces the replica that held offline edits.
#[test]
fn a_replaced_replica_offers_the_offline_edits() {
    let mut room = Room::new(2);
    let m = mem(&room, 0);
    room.relay.drop_link(&m);
    room.pump(0);
    room.run(0, "cell.set", json!({"cell": "E5", "input": "offline"})).unwrap();
    room.pump(0);
    room.relay.restore_link(&m);
    room.relay.fall_behind(&m);
    room.pump(0);
    let copy = room.peers[0].live.take_unmerged().expect("the copy");
    assert_eq!(copy_value(&copy, "E5"), Value::Text("offline".into()));
    assert!(told(&room, 0), "told");
}

/// Refused: the relay refuses an edit for good (a read-only ticket); the replica is given up.
#[test]
fn a_refused_edit_is_offered_as_the_unmerged_copy() {
    let mut room = Room::new(2);
    type_unconfirmed(&mut room, "refused");
    let local = *room.peers[0].locals.borrow().last().expect("a submit");
    room.relay.inject(&mem(&room, 0), ServerMsg::Error { doc: ROOM.into(), detail: "read-only access".into(), local: Some(local), code: None });
    room.pump(0);
    room.pump(0);
    assert!(matches!(room.peers[0].live.status(), LiveStatus::Broken(_)), "{:?}", room.peers[0].live.status());
    let copy = room.peers[0].live.take_unmerged().expect("the copy");
    assert_eq!(copy_value(&copy, "E5"), Value::Text("refused".into()));
}

/// Damaged: yrs failed inside a remote update (a caught panic) and the replica is given up.
#[test]
fn a_damaged_replica_offers_the_unconfirmed_edits() {
    let mut room = Room::new(2);
    type_unconfirmed(&mut room, "kept");
    room.relay.refuse_next_submits(0);
    room.run(1, "cell.set", json!({"cell": "A9", "input": "remote"})).unwrap();
    room.pump(1);
    gridcraft_live::damage_next_apply();
    room.pump(0);
    room.pump(0);
    let copy = room.peers[0].live.take_unmerged().expect("the copy");
    assert_eq!(copy_value(&copy, "E5"), Value::Text("kept".into()));
    assert!(!matches!(room.peers[0].live.status(), LiveStatus::Live { .. }), "the replica is given up");
}

/// Resync: updates wait for one that never comes until the replica's bound, so it can only be
/// rebuilt from a full state and the window must reopen.
#[test]
fn a_replica_that_needs_a_resync_offers_the_unconfirmed_edits() {
    let mut room = Room::new(1);
    let mut mallory = room.relay.connect("mallory", "Mallory", true);
    mallory.send(ClientMsg::Join { doc: ROOM.into(), display: "Mallory".into() }).unwrap();
    let space = mallory
        .poll()
        .into_iter()
        .find_map(|m| if let ServerMsg::Joined { space, .. } = m { Some(space) } else { None })
        .expect("the relay granted a space");
    let mut raw = GridDoc::from_state(&room.replica_state(0)).unwrap();
    raw.set_client(space).unwrap();
    let (sheet, key) = (Id::base(0), cell_key(Id::base(20), Id::base(20)));
    let content = |n: i64| CellContent { v: Some(json!(n)), u: Some("mallory".into()), ..CellContent::default() };
    // The write every later one names as its origin; the room never gets it.
    let _withheld = raw.write(|t| t.set_cell(sheet, &key, &content(-1))).unwrap();
    for n in 0..4200 {
        let u = raw.write(|t| t.set_cell(sheet, &key, &content(n))).unwrap();
        let params = serde_json::to_value(TextPayload::new(&u)).unwrap();
        mallory
            .send(ClientMsg::Submit {
                doc: ROOM.into(),
                local: n as u64 + 1,
                command: craft_collab::CRDT_TEXT.into(),
                params,
                scope: None,
                fence: None,
            })
            .unwrap();
    }
    type_unconfirmed(&mut room, "mine");
    room.pump(0);
    room.pump(0);
    assert!(room.peers[0].live.replica().is_some_and(GridDoc::needs_resync), "the replica is stuck");
    let copy = room.peers[0].live.take_unmerged().expect("the copy");
    assert_eq!(copy_value(&copy, "E5"), Value::Text("mine".into()));
}

/// C1: the host ends the session (the page reopened the file in another room) while offline
/// edits wait: they come back as the copy.
#[test]
fn a_host_end_while_offline_keeps_the_offline_edits() {
    let mut room = Room::new(2);
    let m = mem(&room, 0);
    room.relay.drop_link(&m);
    room.pump(0);
    room.run(0, "cell.set", json!({"cell": "E5", "input": "offline"})).unwrap();
    let p = &mut room.peers[0];
    p.live.end(&mut p.session, "The live session was restarted. Reopen the file.");
    room.pump(0);
    assert!(matches!(room.peers[0].live.status(), LiveStatus::Broken(_)), "the link ended");
    let copy = room.peers[0].live.take_unmerged().expect("the copy");
    assert_eq!(copy_value(&copy, "E5"), Value::Text("offline".into()));
}

/// The same with the link up: an edit sent to a room that is being left may never be ordered.
#[test]
fn a_host_end_right_after_an_edit_keeps_it() {
    let mut room = Room::new(2);
    room.run(0, "cell.set", json!({"cell": "E5", "input": "unconfirmed"})).unwrap();
    let p = &mut room.peers[0];
    p.live.end(&mut p.session, "Your access changed. Reopen the file.");
    let copy = room.peers[0].live.take_unmerged().expect("the copy");
    assert_eq!(copy_value(&copy, "E5"), Value::Text("unconfirmed".into()));
}

/// A host end after everything was confirmed makes no copy.
#[test]
fn a_host_end_with_nothing_unconfirmed_makes_no_copy() {
    let mut room = Room::new(2);
    room.run(0, "cell.set", json!({"cell": "E5", "input": "shared"})).unwrap();
    room.settle();
    let p = &mut room.peers[0];
    p.live.end(&mut p.session, "Your access changed. Reopen the file.");
    room.pump(0);
    assert!(room.peers[0].live.take_unmerged().is_none(), "no copy");
    assert!(!told(&room, 0), "no 'could not be merged'");
}

/// Stopping the session (the workbook closed) while edits wait for the link: stop returns them.
#[test]
fn stopping_while_reconnecting_returns_the_offline_edits() {
    let mut room = Room::new(2);
    let m = mem(&room, 0);
    room.relay.drop_link(&m);
    room.pump(0);
    room.run(0, "cell.set", json!({"cell": "E5", "input": "unsent"})).unwrap();
    let p = room.peers.remove(0);
    let (live, mut session) = (p.live, p.session);
    let copy = live.stop(&mut session).expect("stop returns the copy");
    assert_eq!(copy_value(&copy, "E5"), Value::Text("unsent".into()));
    assert!(copy.0.starts_with(FILE.trim_end_matches(".xlsx")), "{}", copy.0);
}
