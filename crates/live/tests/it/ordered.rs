//! Ordered edits between peers: row and column insert and delete under the structure lock.

use std::sync::{Arc, Mutex};

use craft_collab::Replica;
use craft_live::Access;
use gridcraft_core::Value;
use gridcraft_engine::{Session, io};
use gridcraft_live::{Id, axis_kind_rows, workbook_from_state};
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
        let rebuilt = workbook_from_state(&base, &room.replica_state(i), None).unwrap();
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
        assert_eq!(formula_at(&room, i, "E1").as_deref(), Some("SUM(B2:B9)").or(formula_at(&room, i, "E1").as_deref()));
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
        same_everywhere(&room, &format!("seed {seed}"));
    }
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
    let mut room = Room::new(2);
    let now = Arc::new(Mutex::new(0.0f64));
    let clock = now.clone();
    room.peers[0].live.set_clock(Box::new(move || *clock.lock().unwrap()));
    room.peers[1].live.core_mut().lock("grid/struct").unwrap();
    room.settle();
    let before = room.finger(0);
    room.run(0, "home.insertRows", json!({"rows": "2:2"})).unwrap();
    room.settle();
    let line = room.peers[0].live.status_line().expect("waiting");
    assert!(line.contains("Bob"), "{line}");
    *now.lock().unwrap() = 11_000.0;
    room.settle();
    let told = room.notices_of(0).iter().any(|n| n.contains("Bob") && n.contains("Try again"));
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
