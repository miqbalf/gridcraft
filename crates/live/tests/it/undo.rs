//! Per-user undo and redo: each editor takes back only their own change, as a new edit.

use craft_collab::{ClientMsg, Hub};
use craft_live::testing::ROOM;
use gridcraft_core::{CellRef, Value};
use gridcraft_engine::io;
use gridcraft_live::{volatile_for, workbook_from_state};
use gridcraft_model::{Cell, Formula, Workbook};
use serde_json::json;

use crate::support::{FILE, Room, a1, base_of, diff, finger};

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

fn undo(room: &mut Room, i: usize) {
    room.run(i, "edit.undo", json!({})).unwrap();
    room.settle();
}

fn redo(room: &mut Room, i: usize) {
    room.run(i, "edit.redo", json!({})).unwrap();
    room.settle();
}

#[test]
fn undo_reverts_only_my_change() {
    let mut room = Room::new(2);
    let start = room.finger(0);
    let base_b2 = value_at(&room, 0, "B2");
    room.run(0, "cell.set", json!({"cell": "B2", "input": "100"})).unwrap();
    room.settle();
    // Bob edits the next cell after Alice's change: her undo must leave it alone.
    room.run(1, "cell.set", json!({"cell": "B3", "input": "7"})).unwrap();
    room.settle();
    undo(&mut room, 0);
    for i in 0..2 {
        assert_eq!(value_at(&room, i, "B2"), base_b2, "peer {i}: Alice's change is taken back");
        assert_eq!(value_at(&room, i, "B3"), Value::Number(7.0), "peer {i}: Bob's cell is untouched");
    }
    assert!(room.notices_of(0).iter().all(|n| !n.contains("not undone")), "{:?}", room.notices_of(0));
    same_everywhere(&room, "after the undo");
    // Bob's own undo takes back his cell only.
    undo(&mut room, 1);
    for i in 0..2 {
        assert_eq!(value_at(&room, i, "B3"), Value::Empty, "peer {i}");
        assert_eq!(value_at(&room, i, "B2"), base_b2, "peer {i}");
    }
    assert_eq!(room.finger(0), start);
    same_everywhere(&room, "after both undos");
}

#[test]
fn undo_skips_a_key_someone_changed_since_and_says_who() {
    let mut room = Room::new(2);
    room.run(0, "range.setValues", json!({"range": "D6:D7", "values": [[1], [2]]})).unwrap();
    room.run(0, "home.bold", json!({"range": "E2"})).unwrap();
    room.settle();
    room.run(1, "cell.set", json!({"cell": "D7", "input": "99"})).unwrap();
    room.run(1, "home.italic", json!({"range": "E2"})).unwrap();
    room.settle();
    // The bold first: its key now holds Bob's format.
    undo(&mut room, 0);
    let told = room.notices_of(0).iter().any(|n| n.contains("E2 was changed by someone else since; not undone"));
    assert!(told, "{:?}", room.notices_of(0));
    // Then the values: D6 goes back, D7 keeps Bob's 99, and Alice is told who.
    undo(&mut room, 0);
    for i in 0..2 {
        assert_eq!(value_at(&room, i, "D6"), Value::Empty, "peer {i}");
        assert_eq!(value_at(&room, i, "D7"), Value::Number(99.0), "peer {i}");
    }
    let told = room.notices_of(0).iter().any(|n| n.contains("Bob changed D7 since; not undone"));
    assert!(told, "{:?}", room.notices_of(0));
    assert!(room.notices_of(1).iter().all(|n| !n.contains("not undone")), "only the one who undid is told");
    same_everywhere(&room, "after the skipped undo");
}

#[test]
fn undo_of_insert_deletes_my_rows_on_every_peer() {
    let mut room = Room::new(2);
    let base = room.finger(0);
    room.run(0, "home.insertRows", json!({"rows": "3:4"})).unwrap();
    room.settle();
    assert_ne!(room.finger(1), base, "the insert reached Bob");
    undo(&mut room, 0);
    for i in 0..2 {
        assert_eq!(room.finger(i), base, "peer {i}:\n{}", diff(&room.finger(i), &base));
        assert_eq!(value_at(&room, i, "A3"), Value::Number(3.0), "peer {i}");
    }
    same_everywhere(&room, "after undoing the insert");
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
fn undo_of_delete_restores_rows_and_references() {
    let mut room = Room::with(2, base_with_ref(), None);
    room.run(0, "home.bold", json!({"range": "B9"})).unwrap();
    room.run(0, "home.rowHeight", json!({"rows": "9:9", "height": 33})).unwrap();
    room.run(1, "cell.set", json!({"cell": "D9", "input": "typed"})).unwrap();
    room.settle();
    let before = room.finger(0);
    room.run(0, "home.deleteRows", json!({"rows": "9:9"})).unwrap();
    room.settle();
    for i in 0..2 {
        assert!(formula_at(&room, i, "C2").is_some_and(|f| f.contains("#REF!")), "peer {i}");
    }
    undo(&mut room, 0);
    for i in 0..2 {
        assert_eq!(room.finger(i), before, "peer {i}:\n{}", diff(&room.finger(i), &before));
        assert_eq!(formula_at(&room, i, "C2").as_deref(), Some("B9"), "peer {i}");
        assert_eq!(value_at(&room, i, "D9"), Value::Text("typed".into()), "peer {i}: Bob's cell in the row came back too");
    }
    same_everywhere(&room, "after undoing the delete");
}

#[test]
fn undo_of_a_delete_someone_built_on_is_refused_with_their_name() {
    let mut room = Room::new(2);
    room.run(0, "home.deleteRows", json!({"rows": "5:5"})).unwrap();
    room.settle();
    // Bob inserts a row where Alice's deleted row was.
    room.run(1, "home.insertRows", json!({"rows": "5:5"})).unwrap();
    room.settle();
    let before = room.finger(0);
    undo(&mut room, 0);
    let told = room.notices_of(0).iter().any(|n| n.contains("Bob changed these rows since; not undone"));
    assert!(told, "{:?}", room.notices_of(0));
    for i in 0..2 {
        assert_eq!(room.finger(i), before, "peer {i}: nothing was undone");
    }
    same_everywhere(&room, "after the refused undo");
}

#[test]
fn redo_mirrors_undo() {
    let mut room = Room::new(2);
    let f0 = room.finger(0);
    room.run(0, "cell.set", json!({"cell": "D4", "input": "=A1*2"})).unwrap();
    room.settle();
    let f1 = room.finger(0);
    room.run(0, "home.insertRows", json!({"rows": "2:2"})).unwrap();
    room.settle();
    let f2 = room.finger(0);
    assert!(f0 != f1 && f1 != f2);
    for (step, want) in [(false, &f1), (false, &f0), (true, &f1), (true, &f2), (false, &f1), (true, &f2)] {
        if step {
            redo(&mut room, 0);
        } else {
            undo(&mut room, 0);
        }
        for i in 0..2 {
            assert_eq!(&room.finger(i), want, "redo={step} peer {i}:\n{}", diff(&room.finger(i), want));
        }
        same_everywhere(&room, &format!("redo={step}"));
    }
    assert!(room.notices_of(0).iter().all(|n| !n.contains("not undone")), "{:?}", room.notices_of(0));
}

#[test]
fn a_new_command_clears_redo() {
    let mut room = Room::new(2);
    room.run(0, "cell.set", json!({"cell": "D4", "input": "1"})).unwrap();
    room.settle();
    undo(&mut room, 0);
    room.run(0, "cell.set", json!({"cell": "D5", "input": "2"})).unwrap();
    room.settle();
    let before = room.finger(0);
    redo(&mut room, 0);
    for i in 0..2 {
        assert_eq!(value_at(&room, i, "D4"), Value::Empty, "peer {i}: the old redo is gone");
        assert_eq!(room.finger(i), before, "peer {i}");
    }
}

#[test]
fn one_command_is_one_undo_step() {
    let mut room = Room::new(2);
    room.run(0, "cell.set", json!({"cell": "J1", "input": "kept"})).unwrap();
    room.settle();
    let before = room.finger(0);
    let values: Vec<Vec<u32>> = (0..10).map(|r| (0..10).map(|c| r * 10 + c + 1).collect()).collect();
    room.run(0, "range.setValues", json!({"range": "E10:N19", "values": values})).unwrap();
    room.settle();
    assert_ne!(room.finger(1), before);
    undo(&mut room, 0);
    for i in 0..2 {
        assert_eq!(room.finger(i), before, "peer {i}: all 100 cells went back in one step:\n{}", diff(&room.finger(i), &before));
    }
    assert_eq!(value_at(&room, 0, "J1"), Value::Text("kept".into()), "the step before is still there");
}

#[test]
fn one_undo_takes_back_a_100k_cell_paste_sent_in_parts() {
    let mut room = Room::new(2);
    let text = (0..1000).map(|r| (0..100).map(|c| format!("v{r}-{c}")).collect::<Vec<_>>().join("\t")).collect::<Vec<_>>().join("\n");
    let before = room.finger(0);
    room.run(0, "edit.paste", json!({"at": "A20", "text": text})).unwrap();
    room.settle();
    let cmds = room.peers[0].live.take_commands();
    assert!(cmds.iter().any(|c| c.updates > 1), "the paste went out as several updates of one group: {cmds:?}");
    let count = |room: &Room, i: usize| room.wb(i).sheet(0).unwrap().cells.iter().count();
    assert!(
        room.wb(1).sheet(0).unwrap().cell(CellRef::new(1018, 99)).is_some(),
        "the paste arrived: {} and {} cells, {cmds:?}, {:?} {:?}",
        count(&room, 0),
        count(&room, 1),
        room.notices_of(0),
        room.peers[0].live.status()
    );
    undo(&mut room, 0);
    for i in 0..2 {
        assert_eq!(room.finger(i), before, "peer {i}: one undo took the whole paste back");
    }
    let sizes = room.peers[0].sizes.borrow().clone();
    assert!(sizes.iter().all(|s| *s <= gridcraft_live::split::UPDATE_MAX_BYTES), "the undo's updates are within the cap");
}

#[test]
fn undo_of_an_ordered_edit_waits_for_the_structure_lock() {
    let mut room = Room::new(2);
    let base = room.finger(0);
    room.run(0, "home.insertRows", json!({"rows": "3:3"})).unwrap();
    room.settle();
    let inserted = room.finger(0);
    let locks = *room.peers[0].locks.borrow();
    // Carol's window holds the structure lock.
    let mut carol = room.relay.connect("carol", "Carol", true);
    carol.send(ClientMsg::Lock { doc: ROOM.into(), scope: "grid/struct".into() }).unwrap();
    room.settle();
    room.run(0, "edit.undo", json!({})).unwrap();
    room.settle();
    assert!(*room.peers[0].locks.borrow() > locks, "the undo asked for the lock");
    let line = room.peers[0].live.status_line().expect("waiting for the lock");
    assert!(line.contains("Carol"), "{line}");
    assert_eq!(room.finger(0), inserted, "nothing is undone while another holds the lock");
    carol.send(ClientMsg::Unlock { doc: ROOM.into(), scope: "grid/struct".into() }).unwrap();
    room.settle();
    for i in 0..2 {
        assert_eq!(room.finger(i), base, "peer {i}: undone once the lock was free");
    }
    same_everywhere(&room, "after the locked undo");
}

#[test]
fn two_undos_of_my_own_edits_to_one_cell_chain() {
    let mut room = Room::new(2);
    let start = room.finger(0);
    room.run(0, "cell.set", json!({"cell": "D4", "input": "1"})).unwrap();
    room.settle();
    room.run(0, "cell.set", json!({"cell": "D4", "input": "2"})).unwrap();
    room.settle();
    let two = room.finger(0);
    undo(&mut room, 0);
    for i in 0..2 {
        assert_eq!(value_at(&room, i, "D4"), Value::Number(1.0), "peer {i}");
    }
    undo(&mut room, 0);
    for i in 0..2 {
        assert_eq!(room.finger(i), start, "peer {i}: both of my edits are undone:\n{}", diff(&room.finger(i), &start));
    }
    redo(&mut room, 0);
    redo(&mut room, 0);
    for i in 0..2 {
        assert_eq!(room.finger(i), two, "peer {i}: and both come back");
    }
    assert!(room.notices_of(0).iter().all(|n| !n.contains("not undone")), "{:?}", room.notices_of(0));
    same_everywhere(&room, "after chained undos");
}

#[test]
fn a_second_undo_waits_for_a_pending_ordered_undo() {
    let mut room = Room::with(2, base_with_ref(), None);
    let start = room.finger(0);
    room.run(0, "cell.set", json!({"cell": "E2", "input": "=SUM(B2:B9)"})).unwrap();
    room.settle();
    // The delete shrinks E2's range end: E2 is one of its writes too.
    room.run(0, "home.deleteRows", json!({"rows": "9:9"})).unwrap();
    room.settle();
    let mut carol = room.relay.connect("carol", "Carol", true);
    carol.send(ClientMsg::Lock { doc: ROOM.into(), scope: "grid/struct".into() }).unwrap();
    room.settle();
    // Two quick Ctrl+Z while the first (the delete's) waits for Carol's lock.
    room.run(0, "edit.undo", json!({})).unwrap();
    room.run(0, "edit.undo", json!({})).unwrap();
    room.settle();
    assert_ne!(room.finger(0), start, "nothing ran while the lock was held");
    carol.send(ClientMsg::Unlock { doc: ROOM.into(), scope: "grid/struct".into() }).unwrap();
    room.settle();
    room.settle();
    for i in 0..2 {
        assert_eq!(room.finger(i), start, "peer {i}: both undone, in order:\n{}", diff(&room.finger(i), &start));
    }
    assert!(room.notices_of(0).iter().all(|n| !n.contains("not undone")), "{:?}", room.notices_of(0));
    same_everywhere(&room, "after the held undo");
}

#[test]
fn a_batch_with_an_undo_and_a_refused_edit_still_sends_what_it_wrote() {
    let mut room = Room::new(2);
    room.run(0, "cell.set", json!({"cell": "D4", "input": "1"})).unwrap();
    room.settle();
    // The host hands one flush an undo, then two edits the room refuses as one part (the second
    // inserts too many rows).
    let s = &mut room.peers[0].session;
    s.execute("edit.undo", json!({})).unwrap();
    s.execute("cell.set", json!({"cell": "E5", "input": "5"})).unwrap();
    s.execute("home.insertRows", json!({"rows": format!("2:{}", gridcraft_live::LINES_MAX + 2)})).unwrap();
    room.settle();
    for i in 0..2 {
        assert_eq!(value_at(&room, i, "D4"), Value::Empty, "peer {i}: the undo went out");
        assert_eq!(value_at(&room, i, "E5"), Value::Empty, "peer {i}: the refused part is gone everywhere");
        assert_eq!(value_at(&room, i, "A2"), Value::Number(2.0), "peer {i}: the refused insert is not there");
    }
    assert!(room.notices_of(0).iter().any(|n| n.contains("cannot be changed at once")), "{:?}", room.notices_of(0));
    same_everywhere(&room, "after the mixed batch");
}

#[test]
fn a_replaced_replica_clears_the_undo_history() {
    let mut room = Room::new(2);
    room.run(0, "home.insertRows", json!({"rows": "3:3"})).unwrap();
    room.run(0, "cell.set", json!({"cell": "D4", "input": "1"})).unwrap();
    room.settle();
    let state = room.replica_state(0);
    let p = &mut room.peers[0];
    p.live.apply_checkpoint(&mut p.session, &state).unwrap();
    room.settle();
    let before = room.finger(0);
    undo(&mut room, 0);
    undo(&mut room, 0);
    for i in 0..2 {
        assert_eq!(room.finger(i), before, "peer {i}: nothing from before the swap is undone");
    }
    same_everywhere(&room, "after the swap");
}

#[test]
fn an_ordered_undo_that_gave_up_can_be_asked_again() {
    let mut room = Room::new(2);
    let start = room.finger(0);
    let now = std::sync::Arc::new(std::sync::Mutex::new(0.0f64));
    let clock = now.clone();
    room.peers[0].live.set_clock(Box::new(move || *clock.lock().unwrap()));
    room.run(0, "home.insertRows", json!({"rows": "3:3"})).unwrap();
    room.settle();
    let inserted = room.finger(0);
    let mut carol = room.relay.connect("carol", "Carol", true);
    carol.send(ClientMsg::Lock { doc: ROOM.into(), scope: "grid/struct".into() }).unwrap();
    room.settle();
    undo(&mut room, 0);
    *now.lock().unwrap() = 11_000.0;
    room.settle();
    assert!(room.notices_of(0).iter().any(|n| n.contains("Carol") && n.contains("was not made")), "{:?}", room.notices_of(0));
    assert_eq!(room.finger(0), inserted);
    carol.send(ClientMsg::Unlock { doc: ROOM.into(), scope: "grid/struct".into() }).unwrap();
    room.settle();
    undo(&mut room, 0);
    for i in 0..2 {
        assert_eq!(room.finger(i), start, "peer {i}: the step was kept and undone on the second ask");
    }
}
