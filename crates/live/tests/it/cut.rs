//! Cut then paste while editing live (GX1): the move is shared as cell writes, the same move the
//! desktop makes, one undo step for the peer who made it.

use gridcraft_core::Value;
use gridcraft_engine::{DocState, Session, Trust, io};
use gridcraft_live::{volatile_for, workbook_from_state};
use serde_json::{Value as Json, json};

use crate::support::{FILE, Room, a1, base_bytes, diff, finger};

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
        assert_eq!(room.finger(i), finger(&rebuilt), "{what}: peer {i} differs from its state:\n{}", diff(&room.finger(i), &finger(&rebuilt)));
    }
}

fn cell_on(room: &Room, i: usize, sheet: usize, cell: &str) -> (Value, Option<String>) {
    room.wb(i)
        .sheet(sheet)
        .and_then(|s| s.cell(a1(cell)))
        .map_or((Value::Empty, None), |c| (c.value.clone(), c.formula.as_ref().map(|f| f.text.clone())))
}

/// The same commands on a desktop session (no live editing): what the move must give.
fn desktop(steps: &[(&str, Json)]) -> String {
    let (wb, _) = io::open_bytes(FILE, &base_bytes(false)).unwrap();
    let mut s = Session::new();
    s.add_document(DocState::new(wb, None, "desktop".to_owned(), Trust::FromFile));
    for (id, p) in steps {
        s.execute(id, p.clone()).unwrap_or_else(|e| panic!("desktop {id}: {e}"));
    }
    finger(&s.active().unwrap().wb)
}

fn run_all(room: &mut Room, i: usize, steps: &[(&str, Json)]) {
    for (id, p) in steps {
        room.run(i, id, p.clone()).unwrap_or_else(|e| panic!("{id}: {e}"));
    }
}

#[test]
fn cut_and_paste_moves_the_block_on_every_peer_and_undo_puts_it_back() {
    let mut room = Room::new(2);
    let steps: Vec<(&str, Json)> = vec![
        // Formulas outside the block that point into it, and one inside it that points out.
        ("cell.set", json!({"cell": "G1", "input": "=A2+B2"})),
        ("cell.set", json!({"cell": "G2", "input": "=SUM(A1:B2)"})),
        ("home.bold", json!({"range": "A2"})),
        ("edit.cut", json!({"range": "A1:B2"})),
        ("selection.set", json!({"range": "D5"})),
        ("edit.paste", json!({})),
    ];
    run_all(&mut room, 0, &steps[..3]);
    room.settle();
    let before = room.finger(0);
    run_all(&mut room, 0, &steps[3..]);
    room.settle();
    let want = desktop(&steps);
    assert_eq!(room.finger(0), want, "the live move is the desktop move:\n{}", diff(&want, &room.finger(0)));
    same_everywhere(&room, "after the move");
    for i in 0..2 {
        assert_eq!(cell_on(&room, i, 0, "D6").0, Value::Number(2.0), "peer {i}: A2's value is at D6");
        assert_eq!(cell_on(&room, i, 0, "E6").0, Value::Number(42.0), "peer {i}: B2's value is at E6");
        assert!(cell_on(&room, i, 0, "E5").1.is_some(), "peer {i}: B1's formula is at E5");
        for c in ["A1", "A2", "B1", "B2"] {
            assert_eq!(cell_on(&room, i, 0, c), (Value::Empty, None), "peer {i}: {c} is empty after the cut");
        }
        assert_eq!(cell_on(&room, i, 0, "G1").1.as_deref(), Some("D6+E6"), "peer {i}: references follow the move");
        assert_eq!(cell_on(&room, i, 0, "G2").1.as_deref(), Some("SUM(D5:E6)"), "peer {i}");
    }
    assert!(room.notices_of(0).is_empty(), "no notice for a move: {:?}", room.notices_of(0));
    // One undo step takes the whole move back, on every peer.
    room.run(0, "edit.undo", json!({})).unwrap();
    room.settle();
    assert_eq!(room.finger(0), before, "undo puts the block back:\n{}", diff(&before, &room.finger(0)));
    same_everywhere(&room, "after the undo");
}

#[test]
fn cut_and_paste_to_another_sheet_is_the_desktop_move() {
    let mut room = Room::new(2);
    let steps: Vec<(&str, Json)> = vec![
        ("cell.set", json!({"cell": "G1", "input": "=A2*10"})),
        ("edit.cut", json!({"range": "A1:A2"})),
        ("sheet.activate", json!({"sheet": 1})),
        ("selection.set", json!({"range": "B3"})),
        ("edit.paste", json!({})),
    ];
    run_all(&mut room, 0, &steps);
    room.settle();
    let want = desktop(&steps);
    assert_eq!(room.finger(0), want, "the live move is the desktop move:\n{}", diff(&want, &room.finger(0)));
    same_everywhere(&room, "after the move to another sheet");
    for i in 0..2 {
        assert_eq!(cell_on(&room, i, 1, "B4").0, Value::Number(2.0), "peer {i}");
        assert_eq!(cell_on(&room, i, 0, "A2"), (Value::Empty, None), "peer {i}");
    }
}

#[test]
fn a_move_of_a_note_is_refused_whole_and_nothing_changes() {
    // A note on the block: notes are not shared live yet, so the move is refused as one, never
    // half done (the cells moved and the note left behind).
    let (mut wb, _) = io::open_bytes(FILE, &base_bytes(false)).unwrap();
    let note = gridcraft_model::Comment {
        author: "Alice".into(),
        text: "a note".into(),
        replies: Vec::new(),
        threaded: false,
        resolved: false,
        visible: false,
        dates: Vec::new(),
        note_box: None,
    };
    wb.sheet_mut(0).unwrap().comments.insert(a1("A1"), note);
    let mut room = Room::with(2, io::save_bytes(&wb, FILE).unwrap(), None);
    let (before, seq) = (room.finger(0), room.relay.seq());
    room.run(0, "edit.cut", json!({"range": "A1:A2"})).unwrap();
    room.select(0, "D5");
    let r = room.run(0, "edit.paste", json!({}));
    room.settle();
    assert!(r.is_err() || !room.notices_of(0).is_empty(), "the person is told: {r:?}");
    for i in 0..2 {
        assert_eq!(room.finger(i), before, "peer {i}: nothing moved:\n{}", diff(&before, &room.finger(i)));
    }
    assert_eq!(room.relay.seq(), seq, "nothing was sent");
}
