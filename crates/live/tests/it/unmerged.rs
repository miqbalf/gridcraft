//! The unmerged copy (G16 final): every path on which craft-live drops local edits the room does
//! not have hands the host a copy of the workbook with them, and a session the host ends or stops
//! keeps the edits the room never confirmed.

use chrono::NaiveDateTime;
use craft_collab::{ClientMsg, Hub, Replica, ServerMsg, TextPayload};
use craft_live::LiveStatus;
use craft_live::testing::ROOM;
use gridcraft_core::Value;
use gridcraft_live::{CellContent, GridDoc, Id, RECOVERY_TEXT, UNMERGED_TEXT, UnmergedCopy, cell_key, unmerged_name};
use serde_json::json;

use crate::support::{FILE, Room, a1};

/// The value at `cell` of the first sheet of the copy's workbook.
fn copy_value(copy: &UnmergedCopy, cell: &str) -> Value {
    let (wb, _) = gridcraft_engine::io::open_bytes(&copy.name, &copy.bytes).expect("the copy opens");
    wb.sheet(0).and_then(|s| s.cell(a1(cell))).map(|c| c.value.clone()).unwrap_or(Value::Empty)
}

fn told(room: &Room, i: usize) -> bool {
    room.notices_of(i).iter().any(|n| n == UNMERGED_TEXT)
}

fn mem(room: &Room, i: usize) -> craft_live::testing::MemHub {
    room.peers[i].live.core().hub().inner().mem().clone()
}

/// The one workbook copy peer 0 hands over now.
fn one_copy(room: &mut Room) -> UnmergedCopy {
    let copies = room.take_copies(0);
    assert_eq!(copies.len(), 1, "one copy: {:?}", copies.iter().map(|c| &c.name).collect::<Vec<_>>());
    copies.into_iter().next().unwrap()
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
    let at = NaiveDateTime::parse_from_str("2026-11-24 140405", "%Y-%m-%d %H%M%S").unwrap();
    assert_eq!(unmerged_name("Book.xlsx", at), "Book (unmerged 2026-11-24 140405).xlsx");
    assert_eq!(unmerged_name("dir/Q3 plan.v2.xlsx", at), "Q3 plan.v2 (unmerged 2026-11-24 140405).xlsx");
}

/// M3: two copies made in the same second never share a name.
#[test]
fn copies_made_together_get_distinct_names() {
    let mut room = Room::new(2);
    let at = NaiveDateTime::parse_from_str("2026-11-24 140405", "%Y-%m-%d %H%M%S").unwrap();
    room.peers[0].live.set_wall_clock(Box::new(move || at));
    let mut names = Vec::new();
    for text in ["first", "second"] {
        room.run(0, "cell.set", json!({"cell": "E5", "input": text})).unwrap();
        let state = room.replica_state(0);
        room.peers[0].live.keep_unmerged_state(state);
        names.extend(room.take_copies(0).into_iter().map(|c| c.name));
    }
    assert_eq!(names.len(), 2, "{names:?}");
    assert_ne!(names[0], names[1], "same second, different names");
    assert!(names.iter().all(|n| n.contains("2026-11-24 140405")), "{names:?}");
}

/// I1: a kept state that builds into no workbook does not sink the others: they still make the
/// workbook copy, it comes back as a recovery file, and "a copy is ready" is said only for what
/// exists.
#[test]
fn an_unbuildable_state_does_not_lose_the_others() {
    let mut room = Room::new(2);
    let m = mem(&room, 0);
    room.relay.drop_link(&m);
    room.pump(0);
    room.run(0, "cell.set", json!({"cell": "E5", "input": "offline"})).unwrap();
    room.pump(0);
    let junk = b"not a yrs update".to_vec();
    room.peers[0].live.keep_unmerged_state(junk.clone());
    room.relay.restore_link(&m);
    room.relay.fall_behind(&m);
    room.pump(0);
    let copies = room.take_copies(0);
    let book = copies.iter().find(|c| !c.recovery).expect("the good edits still make a workbook copy");
    assert_eq!(copy_value(book, "E5"), Value::Text("offline".into()));
    let raw = copies.iter().find(|c| c.recovery).expect("the bad state is not dropped");
    assert_eq!(raw.bytes, junk, "handed over as it was kept");
    assert_ne!(raw.name, book.name);
    assert!(told(&room, 0), "the workbook copy exists, so it is told");
    assert!(room.notices_of(0).iter().any(|n| n == RECOVERY_TEXT), "{:?}", room.notices_of(0));
    assert!(room.take_copies(0).is_empty(), "taken once");
}

/// I1: nothing is told before a copy exists.
#[test]
fn no_copy_is_announced_before_it_is_built() {
    let mut room = Room::new(2);
    let m = mem(&room, 0);
    room.relay.drop_link(&m);
    room.pump(0);
    room.run(0, "cell.set", json!({"cell": "E5", "input": "offline"})).unwrap();
    room.pump(0);
    room.relay.restore_link(&m);
    room.relay.fall_behind(&m);
    room.pump(0);
    assert!(room.peers[0].live.has_unmerged(), "kept");
    assert!(!told(&room, 0), "not announced while only kept: {:?}", room.notices_of(0));
    let _ = one_copy(&mut room);
    assert!(told(&room, 0));
}

/// I2: closing the workbook while connected, with an edit in its retry backoff, an ordered run
/// whose submit waits for its retry, and an edit the engine queued in the closing frame: all of
/// them come back in the copy.
#[test]
fn closing_with_queued_and_backoff_edits_keeps_them_all() {
    let mut room = Room::new(2);
    let base_a3 = room.base().sheet(0).unwrap().cell(a1("A3")).unwrap().value.clone();
    // Refused for now: each waits for its retry.
    room.relay.refuse_next_submits(usize::MAX);
    room.run(0, "cell.set", json!({"cell": "E5", "input": "retry"})).unwrap();
    room.pump(0);
    room.run(0, "home.insertRows", json!({"rows": "3:3"})).unwrap();
    for _ in 0..4 {
        room.pump(0);
    }
    assert!(matches!(room.peers[0].live.status(), LiveStatus::Live { .. }), "still live and connected");
    // Typed straight into the engine: the observer queued it, nothing flushed it yet.
    let p = &mut room.peers[0];
    p.session.execute("cell.set", json!({"cell": "G9", "input": "queued"})).unwrap();
    p.live.run(&mut p.session, "file.close", json!({"force": true})).unwrap();
    let p = room.peers.remove(0);
    let (live, mut session) = (p.live, p.session);
    let copies = live.stop(&mut session);
    assert_eq!(copies.len(), 1, "one copy");
    let copy = &copies[0];
    assert_eq!(copy_value(copy, "E6"), Value::Text("retry".into()), "the edit in backoff (moved down by the insert)");
    assert_eq!(copy_value(copy, "A4"), base_a3, "the ordered run");
    assert_eq!(copy_value(copy, "G9"), Value::Text("queued".into()), "the edit queued in the closing frame");
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
    let copy = one_copy(&mut room);
    assert!(copy.taken_back, "a run taken back: offered, never saved unasked");
    assert!(!told(&room, 0), "the take-back notice offers it, with no second notice: {:?}", room.notices_of(0));
    assert!(room.notices_of(0).iter().any(|n| gridcraft_live::TAKE_BACK_TEXTS.contains(&n.as_str())), "{:?}", room.notices_of(0));
    assert_eq!(copy_value(&copy, "A4"), base_a3, "the copy holds the inserted row");
    assert_eq!(copy_value(&copy, "A3"), Value::Empty);
    assert!(room.take_copies(0).is_empty(), "taken once");
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
    let copy = one_copy(&mut room);
    assert_eq!(copy_value(&copy, "E5"), Value::Text("offline".into()));
    assert!(!copy.taken_back);
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
    let copy = one_copy(&mut room);
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
    let copy = one_copy(&mut room);
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
    let copy = one_copy(&mut room);
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
    let copy = one_copy(&mut room);
    assert_eq!(copy_value(&copy, "E5"), Value::Text("offline".into()));
}

/// The same with the link up: an edit sent to a room that is being left may never be ordered.
#[test]
fn a_host_end_right_after_an_edit_keeps_it() {
    let mut room = Room::new(2);
    room.run(0, "cell.set", json!({"cell": "E5", "input": "unconfirmed"})).unwrap();
    let p = &mut room.peers[0];
    p.live.end(&mut p.session, "Your access changed. Reopen the file.");
    let copy = one_copy(&mut room);
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
    assert!(room.take_copies(0).is_empty(), "no copy");
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
    let copies = live.stop(&mut session);
    let copy = copies.first().expect("stop returns the copy");
    assert_eq!(copy_value(copy, "E5"), Value::Text("unsent".into()));
    assert!(copy.name.starts_with(FILE.trim_end_matches(".xlsx")), "{}", copy.name);
}
