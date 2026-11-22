//! Ordered edits between peers: row and column insert and delete under the structure lock.

use std::sync::{Arc, Mutex};

use craft_collab::{ClientMsg, Hub, Replica};
use craft_live::Access;
use craft_live::testing::ROOM;
use gridcraft_core::Value;
use gridcraft_engine::{Session, io};
use gridcraft_live::{Id, LINES_MAX, TOO_MANY_TEXT, axis_kind_rows, volatile_for, workbook_from_state};
use gridcraft_model::{Cell, Formula, Workbook};
use serde_json::{Value as Json, json};

use crate::support::{FILE, Room, a1, base_of, diff, finger};
use crate::{Rng, seeds};

/// What a plain engine session gives for the same commands: the relation every peer must meet.
fn reference(bytes: &[u8], cmds: &[(&str, Json)]) -> String {
    let (wb, _) = io::open_bytes(FILE, bytes).unwrap();
    let mut s = Session::new();
    s.add_document(gridcraft_engine::DocState::new(wb, None, "ref".into(), gridcraft_engine::Trust::FromFile));
    for (id, p) in cmds {
        s.execute(id, p.clone()).unwrap();
    }
    finger(&s.active().unwrap().wb)
}

fn same_everywhere(room: &Room, what: &str) {
    let f0 = room.finger(0);
    for i in 1..room.peers.len() {
        assert_eq!(f0, room.finger(i), "{what}: peers 0 and {i} differ:\n{}", diff(&f0, &room.finger(i)));
    }
    let base = room.base();
    for i in 0..room.peers.len() {
        let state = room.replica_state(i);
        let source = volatile_for(&base, &state).unwrap();
        let rebuilt = workbook_from_state(&base, &state, Some(source)).unwrap();
        assert_eq!(room.finger(i), finger(&rebuilt), "{what}: peer {i} differs from the state:\n{}", diff(&room.finger(i), &finger(&rebuilt)));
    }
}

fn value_at(room: &Room, i: usize, cell: &str) -> Value {
    room.wb(i).sheet(0).and_then(|s| s.cell(a1(cell))).map(|c| c.value.clone()).unwrap_or(Value::Empty)
}

fn formula_at(room: &Room, i: usize, cell: &str) -> Option<String> {
    room.wb(i).sheet(0).and_then(|s| s.cell(a1(cell))).and_then(|c| c.formula.as_ref().map(|f| f.text.clone()))
}

#[test]
fn concurrent_typing_and_row_insert_land_in_the_typed_row() {
    for typed_first in [true, false] {
        let mut room = Room::new(2);
        if typed_first {
            room.run(0, "cell.set", json!({"cell": "B2", "input": "100"})).unwrap();
            room.run(1, "home.insertRows", json!({"rows": "2:2"})).unwrap();
        } else {
            room.run(1, "home.insertRows", json!({"rows": "2:2"})).unwrap();
            room.pump(1);
            room.run(0, "cell.set", json!({"cell": "B2", "input": "100"})).unwrap();
        }
        room.settle();
        for i in 0..2 {
            assert_eq!(value_at(&room, i, "B3"), Value::Number(100.0), "typed_first={typed_first} peer {i}");
            assert_eq!(value_at(&room, i, "B2"), Value::Empty, "typed_first={typed_first} peer {i}");
        }
        same_everywhere(&room, "typing and insert");
    }
}

#[test]
fn sum_follows_a_row_delete_on_every_peer() {
    let mut room = Room::new(2);
    room.run(0, "cell.set", json!({"cell": "E1", "input": "=SUM(B2:B10)"})).unwrap();
    room.settle();
    room.run(1, "home.deleteRows", json!({"rows": "5:5"})).unwrap();
    room.settle();
    let want = reference(&room.bytes, &[("cell.set", json!({"cell": "E1", "input": "=SUM(B2:B10)"})), ("home.deleteRows", json!({"rows": "5:5"}))]);
    for i in 0..2 {
        assert_eq!(room.finger(i), want, "peer {i}");
        assert_eq!(formula_at(&room, i, "E1").as_deref(), Some("SUM(B2:B9)"), "peer {i}");
    }
    assert_eq!(formula_at(&room, 0, "E1"), formula_at(&room, 1, "E1"));
    same_everywhere(&room, "sum and delete");
}

#[test]
fn second_insert_waits_and_lands_where_its_selection_now_is() {
    let mut room = Room::new(2);
    room.select(1, "A5");
    room.run(0, "home.insertRows", json!({"rows": "3:3"})).unwrap();
    // Bob asks while Alice holds the lock; he has no rows named: his selection is the target.
    room.run(1, "home.insertRows", json!({})).unwrap();
    room.settle();
    let want = reference(&room.bytes, &[("home.insertRows", json!({"rows": "3:3"})), ("home.insertRows", json!({"rows": "6:6"}))]);
    for i in 0..2 {
        assert_eq!(room.finger(i), want, "peer {i}");
        // The row Bob had selected (value 5) moved down by both inserts.
        assert_eq!(value_at(&room, i, "A7"), Value::Number(5.0));
    }
    same_everywhere(&room, "two inserts");
}

#[test]
fn receiver_engine_equals_rebuild_from_state_after_each_op() {
    let mut total_ops = 0u32;
    for seed in seeds() {
        let mut rng = Rng(seed ^ 0xA5A5);
        let mut room = Room::new(3);
        let base = room.base();
        for step in 0..30 {
            let who = rng.below(3);
            let row = 1 + rng.below(9);
            let col = (b'A' + u8::try_from(rng.below(4)).unwrap()) as char;
            let (id, p) = match rng.below(12) {
                0 => ("home.insertRows", json!({"rows": format!("{row}:{}", row + rng.below(2))})),
                1 => ("home.deleteRows", json!({"rows": format!("{row}:{row}")})),
                2 => ("home.insertColumns", json!({"cols": format!("{col}:{col}")})),
                3 => ("home.deleteColumns", json!({"cols": format!("{col}:{col}")})),
                4 => ("cell.set", json!({"cell": format!("{col}{row}"), "input": format!("{}", rng.below(500))})),
                5 => ("cell.set", json!({"cell": format!("{col}{row}"), "input": format!("=SUM(A{row}:B{})", row + 2)})),
                6 => ("cell.set", json!({"cell": format!("{col}{row}"), "input": format!("=A{row}+B{}", 1 + rng.below(9))})),
                7 => ("home.rowHeight", json!({"rows": format!("{row}:{row}"), "height": 20 + rng.below(40)})),
                8 => ("home.hideRows", json!({"rows": format!("{row}:{row}")})),
                9 => ("home.bold", json!({"range": format!("{col}{row}:{col}{}", row + 1)})),
                10 => ("edit.clearContents", json!({"range": format!("{col}{row}:{col}{row}")})),
                _ => ("home.columnWidth", json!({"cols": format!("{col}:{col}"), "width": 60 + rng.below(80)})),
            };
            let _ = room.run(who, id, p);
            let mut order: Vec<usize> = (0..3).collect();
            for i in (1..order.len()).rev() {
                order.swap(i, rng.below(i + 1));
            }
            for i in order {
                room.pump(i);
                let state = room.replica_state(i);
                let rebuilt = workbook_from_state(&base, &state, None).unwrap();
                assert_eq!(room.finger(i), finger(&rebuilt), "seed {seed} step {step} peer {i}:\n{}", diff(&room.finger(i), &finger(&rebuilt)));
            }
        }
        room.settle();
        total_ops += room.peers[0].live.replica().unwrap().ops_len();
        same_everywhere(&room, &format!("seed {seed}"));
    }
    assert!(total_ops > 0, "the seeds ran ordered edits");
}

fn base_with_ref() -> Vec<u8> {
    let mut wb: Workbook = base_of(&crate::support::base_bytes(false));
    if let Some(s) = wb.sheet_mut(0) {
        s.set_cell(a1("B9"), Cell::value(Value::Number(9.0)));
        s.set_cell(a1("C2"), Cell::formula(Formula::new("=B9")));
    }
    io::save_bytes(&wb, FILE).unwrap()
}

#[test]
fn a_base_formula_to_a_deleted_row_prints_ref_and_returns_on_undelete() {
    let mut room = Room::with(2, base_with_ref(), None);
    room.run(0, "home.deleteRows", json!({"rows": "9:9"})).unwrap();
    room.settle();
    for i in 0..2 {
        let f = formula_at(&room, i, "C2").unwrap();
        assert!(f.contains("#REF!"), "peer {i}: {f}");
    }
    same_everywhere(&room, "after the delete");
    let p = &mut room.peers[0];
    p.live.undelete_lines(&mut p.session, Id::base(0), axis_kind_rows(), vec![Id::base(8)]).unwrap();
    room.settle();
    for i in 0..2 {
        assert_eq!(formula_at(&room, i, "C2").as_deref(), Some("B9"), "peer {i}");
        assert_eq!(value_at(&room, i, "B9"), Value::Number(9.0), "peer {i}");
    }
    same_everywhere(&room, "after the undelete");
}

#[test]
fn typing_into_a_deleted_row_tells_the_typist_with_the_value() {
    let mut room = Room::new(2);
    room.run(1, "home.deleteRows", json!({"rows": "5:5"})).unwrap();
    room.pump(1);
    room.run(0, "cell.set", json!({"cell": "B5", "input": "777"})).unwrap();
    room.settle();
    let told = room.notices_of(0).iter().any(|n| n.contains("777") && n.contains("Bob"));
    assert!(told, "{:?}", room.notices_of(0));
    same_everywhere(&room, "typing into a deleted row");
}

#[test]
fn a_held_lock_shows_the_holders_name_and_gives_up_after_10s() {
    let mut room = Room::new(1);
    let now = Arc::new(Mutex::new(0.0f64));
    let clock = now.clone();
    room.peers[0].live.set_clock(Box::new(move || *clock.lock().unwrap()));
    // Bob's window holds the structure lock and never lets it go (it never even polls).
    let mut bob = room.relay.connect("bob", "Bob", true);
    bob.send(ClientMsg::Lock { doc: ROOM.into(), scope: "grid/struct".into() }).unwrap();
    room.settle();
    let before = room.finger(0);
    room.run(0, "home.insertRows", json!({"rows": "2:2"})).unwrap();
    room.settle();
    let line = room.peers[0].live.status_line().expect("waiting");
    assert!(line.contains("Bob"), "{line}");
    *now.lock().unwrap() = 11_000.0;
    room.settle();
    let told = room.notices_of(0).iter().any(|n| n.contains("Bob") && n.contains("was not made"));
    assert!(told, "{:?}", room.notices_of(0));
    assert_eq!(room.finger(0), before);
    assert_eq!(room.peers[0].live.status_line(), None);
}

#[test]
fn read_only_peer_never_asks_for_the_lock() {
    let mut room = Room::new(1);
    let i = room.join("Viewer", Access::Read, None);
    room.settle();
    assert!(room.run(i, "home.insertRows", json!({"rows": "2:2"})).is_err());
    room.settle();
    assert_eq!(*room.peers[i].locks.borrow(), 0);
}

#[test]
fn a_checkpoint_joiner_does_not_replay_ops() {
    let mut room = Room::new(2);
    room.run(0, "home.insertRows", json!({"rows": "3:4"})).unwrap();
    room.run(1, "cell.set", json!({"cell": "A3", "input": "5"})).unwrap();
    room.settle();
    room.run(1, "home.deleteRows", json!({"rows": "6:6"})).unwrap();
    room.settle();
    let state = room.replica_state(0);
    let j = room.join("Carol", Access::Edit, None);
    let p = &mut room.peers[j];
    p.live.apply_checkpoint(&mut p.session, &state).unwrap();
    let ops = gridcraft_live::GridDoc::from_state(&state).unwrap().ops_len();
    assert!(ops >= 2);
    assert_eq!(room.peers[j].live.applied_ops(), ops);
    assert_eq!(room.finger(j), room.finger(0));
}

#[test]
fn delete_then_insert_at_range_top_matches_excel() {
    let mut room = Room::new(2);
    let cmds = [
        ("cell.set", json!({"cell": "E1", "input": "=SUM(A2:A6)"})),
        ("home.deleteRows", json!({"rows": "2:2"})),
        ("home.insertRows", json!({"rows": "2:2"})),
    ];
    room.run(0, cmds[0].0, cmds[0].1.clone()).unwrap();
    room.settle();
    room.run(1, cmds[1].0, cmds[1].1.clone()).unwrap();
    room.settle();
    // The delete shrank the range to A2:A5 on every peer.
    for i in 0..2 {
        assert!(formula_at(&room, i, "E1").unwrap().contains("A2:A5"), "peer {i}: {:?}", formula_at(&room, i, "E1"));
    }
    room.run(0, cmds[2].0, cmds[2].1.clone()).unwrap();
    room.settle();
    let want = reference(&room.bytes, &cmds);
    for i in 0..2 {
        assert_eq!(room.finger(i), want, "peer {i}");
        assert!(formula_at(&room, i, "E1").unwrap().contains("A3:A6"), "peer {i}: {:?}", formula_at(&room, i, "E1"));
    }
    same_everywhere(&room, "delete then insert");
}

#[test]
fn random_numbers_agree_after_a_row_insert_above_a_rand_cell() {
    let mut room = Room::new(3);
    room.run(0, "cell.set", json!({"cell": "E5", "input": "=RAND()"})).unwrap();
    room.settle();
    let before = value_at(&room, 1, "E5");
    assert!(matches!(before, Value::Number(_)));
    room.run(2, "home.insertRows", json!({"rows": "2:3"})).unwrap();
    room.settle();
    // Every peer has the same number in the cell's new place (the draw follows the line ids).
    let after = value_at(&room, 0, "E7");
    assert!(matches!(after, Value::Number(_)));
    for i in 0..3 {
        assert_eq!(value_at(&room, i, "E7"), after, "peer {i}");
        assert_eq!(value_at(&room, i, "E5"), Value::Empty, "peer {i}");
    }
    same_everywhere(&room, "rand after insert");
}

#[test]
fn ordered_ops_reach_the_op_log_and_every_peer_applies_each_once() {
    let mut room = Room::new(3);
    room.run(0, "home.insertRows", json!({"rows": "2:2"})).unwrap();
    room.run(1, "home.deleteColumns", json!({"cols": "C:C"})).unwrap();
    room.settle();
    let n = room.peers[0].live.replica().unwrap().ops_len();
    assert_eq!(n, 2);
    for (i, p) in room.peers.iter().enumerate() {
        assert_eq!(p.live.applied_ops(), n, "peer {i}");
    }
}

/// The sizes of the updates peer `i` sent since `from`.
fn sent_since(room: &Room, i: usize, from: usize) -> Vec<usize> {
    room.peers[i].sizes.borrow()[from..].to_vec()
}

fn assert_under_cap(sizes: &[usize], what: &str) {
    assert!(!sizes.is_empty(), "{what}: nothing was sent");
    assert!(sizes.iter().all(|s| *s <= gridcraft_live::split::UPDATE_MAX_BYTES), "{what}: an update is over the relay's cap: {sizes:?}");
}

#[test]
fn a_one_row_insert_far_down_the_sheet_stays_under_the_cap() {
    let mut room = Room::new(2);
    let from = room.peers[0].sizes.borrow().len();
    room.run(0, "home.insertRows", json!({"rows": "150000:150000"})).unwrap();
    room.settle();
    let sizes = sent_since(&room, 0, from);
    assert_under_cap(&sizes, "insert at row 150000");
    assert!(sizes.len() > 1, "the ids up to the row travelled in parts: {sizes:?}");
    let want = reference(&room.bytes, &[("home.insertRows", json!({"rows": "150000:150000"}))]);
    for i in 0..2 {
        assert_eq!(room.finger(i), want, "peer {i}");
    }
    same_everywhere(&room, "far insert");
    // Rows above and below it still move with the next ordered edit.
    room.run(1, "home.deleteRows", json!({"rows": "2:2"})).unwrap();
    room.settle();
    same_everywhere(&room, "delete after far insert");
}

#[test]
fn the_largest_insert_and_delete_stay_under_the_cap() {
    for custom_height in [false, true] {
        let mut room = Room::new(2);
        if custom_height {
            room.run(0, "home.rowHeight", json!({"rows": "1:1", "height": 55})).unwrap();
            room.settle();
        }
        let n = LINES_MAX;
        // Under the custom-height row 1, every new row takes its height.
        let first = if custom_height { 2 } else { 1 };
        let last = first + n - 1;
        let from = room.peers[0].sizes.borrow().len();
        room.run(0, "home.insertRows", json!({"rows": format!("{first}:{last}")})).unwrap();
        room.settle();
        let sizes = sent_since(&room, 0, from);
        eprintln!("insert {n} custom={custom_height}: {sizes:?}");
        assert_under_cap(&sizes, &format!("insert of {n} rows (custom height {custom_height})"));
        same_everywhere(&room, "big insert");
        let from = room.peers[1].sizes.borrow().len();
        room.run(1, "home.deleteRows", json!({"rows": format!("{first}:{last}")})).unwrap();
        room.settle();
        let sizes = sent_since(&room, 1, from);
        eprintln!("delete {n} custom={custom_height}: {sizes:?}");
        assert_under_cap(&sizes, &format!("delete of {n} rows (custom height {custom_height})"));
        same_everywhere(&room, "big delete");
    }
}

/// A 40,000-line delete is 1.2 MB in one update, over the relay's cap.
const _: () = assert!(LINES_MAX < 40_000);

#[test]
fn more_lines_than_the_cap_are_refused_and_nothing_changes() {
    let mut room = Room::new(2);
    let before = room.finger(0);
    // 40,000 lines is what once went out in one update over the relay's cap: it stays refused.
    let over = LINES_MAX + 1;
    for (id, p) in [
        ("home.insertRows", json!({"rows": format!("1:{over}")})),
        ("home.deleteRows", json!({"rows": format!("1:{over}")})),
        ("home.insertRows", json!({"rows": "1:40000"})),
        ("home.deleteRows", json!({"rows": "1:40000"})),
    ] {
        let told = match room.run(0, id, p) {
            Err(e) => e,
            Ok(_) => {
                room.settle();
                room.notices_of(0).join("\n")
            }
        };
        assert!(told.contains(TOO_MANY_TEXT), "{id}: {told}");
    }
    room.settle();
    assert_eq!(room.finger(0), before);
    assert_eq!(room.finger(1), before);
    assert_eq!(room.peers[0].live.replica().unwrap().ops_len(), 0);
    assert!(room.peers[0].sizes.borrow().is_empty(), "nothing was sent");
}

/// Drives peer `i` until its ordered edit has run and gone out, but before the relay's answer
/// is read.
fn until_ran(room: &mut Room, i: usize) {
    for _ in 0..12 {
        if room.peers[i].live.replica().unwrap().ops_len() > 0 {
            return;
        }
        room.pump(i);
    }
    panic!("the ordered edit never ran");
}

#[test]
fn a_rejoin_after_the_edit_ran_is_told_and_the_edit_lands_once() {
    let mut room = Room::new(2);
    room.run(0, "home.insertRows", json!({"rows": "3:3"})).unwrap();
    until_ran(&mut room, 0);
    room.relay.drop_link(room.peers[0].live.core().hub().inner().mem());
    room.relay.restore_link(room.peers[0].live.core().hub().inner().mem());
    room.settle();
    let told = room.notices_of(0).iter().any(|n| n.contains("connection dropped") && n.contains("sent again") && !n.contains("not kept"));
    assert!(told, "{:?}", room.notices_of(0));
    let want = reference(&room.bytes, &[("home.insertRows", json!({"rows": "3:3"}))]);
    for i in 0..2 {
        assert_eq!(room.finger(i), want, "peer {i}: one insert");
    }
    assert_eq!(room.peers[1].live.replica().unwrap().ops_len(), 1);
    same_everywhere(&room, "rejoin");
    // The lock is free again: the next ordered edit goes through.
    room.run(1, "home.deleteRows", json!({"rows": "2:2"})).unwrap();
    room.settle();
    same_everywhere(&room, "after the rejoin");
}

#[test]
fn an_edit_that_waits_for_an_open_undo_group_runs_once_it_closes() {
    let mut room = Room::new(2);
    let token = room.peers[1].session.group_begin("macro").unwrap();
    room.run(0, "cell.set", json!({"cell": "D5", "input": "from alice"})).unwrap();
    room.settle();
    room.run(1, "home.insertRows", json!({"rows": "3:3"})).unwrap();
    for _ in 0..6 {
        room.pump(1);
    }
    assert_eq!(room.peers[1].live.replica().unwrap().ops_len(), 0, "deferred while the group is open");
    room.peers[1].session.group_end(token).unwrap();
    room.settle();
    let want = reference(&room.bytes, &[("cell.set", json!({"cell": "D5", "input": "from alice"})), ("home.insertRows", json!({"rows": "3:3"}))]);
    for i in 0..2 {
        assert_eq!(room.finger(i), want, "peer {i}");
    }
    assert_eq!(room.peers[0].live.replica().unwrap().ops_len(), 1);
    same_everywhere(&room, "deferred edit");
}

#[test]
fn a_deferred_edit_does_not_run_after_a_rejoin_took_its_lock() {
    let mut room = Room::new(2);
    let token = room.peers[1].session.group_begin("macro").unwrap();
    room.run(0, "cell.set", json!({"cell": "D5", "input": "from alice"})).unwrap();
    room.settle();
    room.run(1, "home.insertRows", json!({"rows": "3:3"})).unwrap();
    for _ in 0..6 {
        room.pump(1);
    }
    room.relay.drop_link(room.peers[1].live.core().hub().inner().mem());
    room.relay.restore_link(room.peers[1].live.core().hub().inner().mem());
    room.pump(1);
    room.pump(1);
    room.peers[1].session.group_end(token).unwrap();
    room.settle();
    let told = room.notices_of(1).iter().any(|n| n.contains("connection dropped") && n.contains("Nothing was changed"));
    assert!(told, "{:?}", room.notices_of(1));
    let want = reference(&room.bytes, &[("cell.set", json!({"cell": "D5", "input": "from alice"}))]);
    for i in 0..2 {
        assert_eq!(room.finger(i), want, "peer {i}: the insert never ran");
    }
    assert_eq!(room.peers[0].live.replica().unwrap().ops_len(), 0);
    // The scope is free: Alice's own insert is granted and lands.
    room.run(0, "home.insertRows", json!({"rows": "2:2"})).unwrap();
    room.settle();
    same_everywhere(&room, "after the lost lock");
    assert_eq!(room.peers[0].live.replica().unwrap().ops_len(), 1);
}

#[test]
fn a_refused_ordered_edit_leaves_the_layout_the_engine_and_the_lock_as_before() {
    let mut room = Room::with(2, base_with_ref(), None);
    room.run(0, "home.deleteRows", json!({"rows": "9:9"})).unwrap();
    room.settle();
    let before = room.finger(0);
    let sent = room.peers[0].sizes.borrow().len();
    // An open undo group makes the engine refuse the undelete after the author has begun it.
    let token = room.peers[0].session.group_begin("macro").unwrap();
    let p = &mut room.peers[0];
    p.live.undelete_lines(&mut p.session, Id::base(0), axis_kind_rows(), vec![Id::base(8)]).unwrap();
    for _ in 0..8 {
        room.pump(0);
    }
    let told = room.notices_of(0).iter().any(|n| n.starts_with("Finish what you are doing"));
    assert!(told, "{:?}", room.notices_of(0));
    assert_eq!(room.peers[0].sizes.borrow().len(), sent, "nothing was shared");
    assert_eq!(room.peers[0].live.replica().unwrap().ops_len(), 1, "no undelete in the log");
    room.peers[0].session.group_end(token).unwrap();
    room.settle();
    assert_eq!(room.finger(0), before);
    assert_eq!(room.finger(1), before);
    same_everywhere(&room, "after the refusal");
    assert_eq!(room.peers[0].live.status_line(), None);
    // The lock was given back, and the layout is the document's: a later edit works everywhere.
    room.run(1, "home.deleteRows", json!({"rows": "3:3"})).unwrap();
    room.settle();
    same_everywhere(&room, "an edit after the refusal");
}

/// A far insert after earlier structure edits: the fill-only updates must not make receivers
/// rebuild the engine.
fn far_insert_after(earlier: &[(&str, Json)], what: &str) {
    let mut room = Room::new(3);
    for (id, p) in earlier {
        room.run(0, id, p.clone()).unwrap();
        room.settle();
    }
    let from = room.peers[0].sizes.borrow().len();
    room.run(1, "home.insertRows", json!({"rows": "150000:150000"})).unwrap();
    room.settle();
    assert!(sent_since(&room, 1, from).len() > 1, "{what}: the fill went in parts");
    assert_under_cap(&sent_since(&room, 1, from), what);
    for (i, p) in room.peers.iter().enumerate() {
        assert_eq!(p.live.ordered_rebuilds(), 0, "{what}: peer {i} rebuilt the engine");
    }
    let mut cmds: Vec<(&str, Json)> = earlier.to_vec();
    cmds.push(("home.insertRows", json!({"rows": "150000:150000"})));
    let want = reference(&room.bytes, &cmds);
    for i in 0..3 {
        assert_eq!(room.finger(i), want, "{what}: peer {i}");
    }
    same_everywhere(&room, what);
}

#[test]
fn a_far_insert_after_an_earlier_insert_makes_no_receiver_rebuild() {
    far_insert_after(&[("home.insertRows", json!({"rows": "3:3"}))], "prior insert");
}

#[test]
fn a_far_insert_after_a_delete_in_the_filled_range_makes_no_receiver_rebuild() {
    far_insert_after(&[("home.deleteRows", json!({"rows": "3:3"}))], "prior delete");
}

#[test]
fn a_far_insert_after_an_insert_and_a_delete_makes_no_receiver_rebuild() {
    far_insert_after(
        &[("home.insertRows", json!({"rows": "2:2"})), ("home.deleteRows", json!({"rows": "6:7"})), ("home.insertRows", json!({"rows": "1:1"}))],
        "prior insert and delete",
    );
}

#[test]
fn a_broken_link_after_the_edit_ran_does_not_claim_it_will_be_sent_again() {
    let mut room = Room::new(2);
    room.run(0, "home.insertRows", json!({"rows": "3:3"})).unwrap();
    until_ran(&mut room, 0);
    room.peers[0].live.core_mut().end("The service stopped.");
    for _ in 0..4 {
        room.pump(0);
    }
    let notices = room.notices_of(0);
    let told = notices.iter().any(|n| n.contains("link to the room broke") && n.contains("may not have reached"));
    assert!(told, "{notices:?}");
    assert!(!notices.iter().any(|n| n.contains("sent again")), "{notices:?}");
}
