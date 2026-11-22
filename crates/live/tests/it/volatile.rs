//! Shared random numbers: the same bits on every peer and in a rebuild, new numbers after any
//! edit, the epoch moved by `formulas.calculateNow`, and no epoch writes without a random function.

use craft_collab::Replica;
use gridcraft_calc::VolatileSource;
use gridcraft_core::{CellRef, Value};
use gridcraft_live::{GridDoc, GridSeed, Layout, LiveVolatile, volatile_for, workbook_from_state};
use serde_json::json;

use crate::support::{Room, a1, finger};

fn num(room: &Room, i: usize, cell: &str) -> Value {
    room.wb(i).sheet(0).unwrap().cell(a1(cell)).unwrap().value.clone()
}

fn epoch(room: &Room, i: usize) -> (u64, String) {
    room.peers[i].live.replica().unwrap().meta().epoch
}

#[test]
fn rand_is_equal_on_both_peers_and_in_the_rebuild() {
    let mut room = Room::new(2);
    room.run(0, "cell.set", json!({"cell": "G1", "input": "=RAND()"})).unwrap();
    room.run(1, "cell.set", json!({"cell": "G2", "input": "=RANDBETWEEN(1,1000000)"})).unwrap();
    room.settle();
    for cell in ["G1", "G2"] {
        let (a, b) = (num(&room, 0, cell), num(&room, 1, cell));
        assert!(matches!(a, Value::Number(_)), "{cell} holds a number, got {a:?}");
        match (&a, &b) {
            (Value::Number(x), Value::Number(y)) => assert_eq!(x.to_bits(), y.to_bits(), "{cell}: same bits on both peers"),
            _ => panic!("{cell}: not numbers"),
        }
    }
    let state = room.replica_state(0);
    let base = room.base();
    let rebuilt = workbook_from_state(&base, &state, Some(volatile_for(&base, &state).unwrap())).unwrap();
    for cell in ["G1", "G2"] {
        let (Value::Number(x), Value::Number(y)) = (num(&room, 1, cell), rebuilt.sheet(0).unwrap().cell(a1(cell)).unwrap().value.clone()) else {
            panic!("not numbers")
        };
        assert_eq!(x.to_bits(), y.to_bits(), "{cell}: the rebuild from state shows the same bits");
    }
    assert_eq!(room.finger(0), finger(&rebuilt));
}

#[test]
fn an_edit_by_anyone_changes_the_numbers_for_everyone() {
    let mut room = Room::new(3);
    room.run(0, "cell.set", json!({"cell": "G1", "input": "=RAND()"})).unwrap();
    room.settle();
    let mut seen = vec![num(&room, 0, "G1")];
    for (who, cell) in [(1, "H2"), (2, "H3"), (0, "H4")] {
        room.run(who, "cell.set", json!({"cell": cell, "input": "x"})).unwrap();
        room.settle();
        let now = num(&room, 0, "G1");
        assert!(!seen.contains(&now), "an edit by peer {who} changes the number");
        assert_eq!(now, num(&room, 1, "G1"), "peers 0 and 1 agree");
        assert_eq!(now, num(&room, 2, "G1"), "peers 0 and 2 agree");
        seen.push(now);
    }
    assert_eq!(epoch(&room, 0).0, 4, "one bump for the formula, one per edit");
    assert_eq!(epoch(&room, 0), epoch(&room, 2));
}

#[test]
fn calculate_now_bumps_the_epoch() {
    let mut room = Room::new(2);
    room.run(0, "cell.set", json!({"cell": "G1", "input": "=RAND()"})).unwrap();
    room.settle();
    let (n, _) = epoch(&room, 1);
    let before = num(&room, 1, "G1");
    room.run(1, "formulas.calculateNow", json!({})).unwrap();
    room.settle();
    let (m, by) = epoch(&room, 0);
    assert_eq!(m, n + 1, "the epoch moved by one on every peer");
    assert_eq!(by, "bob", "the bump names who asked");
    assert_ne!(before, num(&room, 1, "G1"));
    assert_eq!(num(&room, 0, "G1"), num(&room, 1, "G1"));
}

#[test]
fn no_random_function_no_epoch_writes() {
    let mut room = Room::new(2);
    let start = epoch(&room, 0);
    room.run(0, "cell.set", json!({"cell": "G1", "input": "=1+2"})).unwrap();
    room.run(1, "cell.set", json!({"cell": "G2", "input": "text"})).unwrap();
    room.run(0, "cell.set", json!({"cell": "G3", "input": "5"})).unwrap();
    room.settle();
    assert_eq!(epoch(&room, 0), start, "the epoch is untouched");
    assert_eq!(epoch(&room, 1), start);
}

#[test]
fn the_same_inputs_give_the_same_bits() {
    let doc = GridDoc::seeded(&GridSeed { base_sha256: [9; 32], sheets: 2 }).unwrap();
    let base = crate::support::base_of(&crate::support::base_bytes(false));
    let (a, b) = (LiveVolatile::new(&doc, &base), LiveVolatile::new(&doc, &base));
    let cell = CellRef::new(3, 4);
    let first = a.random(0, cell, 0);
    assert!((0.0..1.0).contains(&first));
    assert_eq!(first.to_bits(), b.random(0, cell, 0).to_bits());
    assert_ne!(first.to_bits(), a.random(0, cell, 1).to_bits(), "draws differ");
    assert_ne!(first.to_bits(), a.random(1, cell, 0).to_bits(), "sheets differ");
    assert_ne!(first.to_bits(), a.random(0, CellRef::new(4, 3), 0).to_bits(), "rows and columns are not interchangeable");
    // Pinned bits: a change here changes every shared document's numbers, native and wasm alike.
    assert_eq!(first.to_bits(), 4_596_398_138_289_177_264);
    let (seed, n) = a.state();
    assert_ne!(seed, 0);
    assert_eq!(n, 0);
    // The layout is what maps positions to ids.
    a.update_layout(Layout::from_doc(&doc, &base));
    assert_eq!(first.to_bits(), a.random(0, cell, 0).to_bits());
    assert_eq!(doc.state().len(), GridDoc::seeded(&GridSeed { base_sha256: [9; 32], sheets: 2 }).unwrap().state().len());
}
