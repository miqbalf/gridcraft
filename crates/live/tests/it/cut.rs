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

/// Whether peer `i` was told its cut was cancelled, and asked to cut again.
fn told_cut_again(room: &Room, i: usize) -> bool {
    room.notices_of(i).iter().any(|n| n.contains("Cut again"))
}

/// Alice cuts `block`, then `change` happens (bob's edits, or a rejoin), then alice pastes at
/// `at`. A cut whose cells may have moved or changed since is cancelled: the paste moves nothing,
/// the peers agree and alice is told why.
fn cut_cancelled_by(block: &str, at: &str, change: impl FnOnce(&mut Room)) -> Room {
    let mut room = Room::new(2);
    room.run(0, "cell.set", json!({"cell": "B5", "input": "50"})).unwrap();
    room.run(0, "cell.set", json!({"cell": "B6", "input": "60"})).unwrap();
    room.settle();
    room.run(0, "edit.cut", json!({"range": block})).unwrap();
    change(&mut room);
    room.settle();
    let (before, seq) = (room.finger(0), room.relay.seq());
    room.select(0, at);
    let pasted = room.run(0, "edit.paste", json!({}));
    room.settle();
    assert!(told_cut_again(&room, 0), "alice is told the cut was cancelled: {:?}", room.notices_of(0));
    assert!(pasted.is_err() || room.relay.seq() == seq, "the paste did nothing: {pasted:?}");
    assert_eq!(room.relay.seq(), seq, "nothing was sent");
    for i in 0..2 {
        assert_eq!(room.finger(i), before, "peer {i}: nothing moved:\n{}", diff(&before, &room.finger(i)));
    }
    same_everywhere(&room, "after the cancelled cut");
    room
}

#[test]
fn a_cut_held_across_a_peers_row_insert_is_cancelled() {
    // GC1 I1: bob's insert moves the block down a row; a paste by position would take the wrong
    // cells (the old row 4 and half the block).
    let room = cut_cancelled_by("A5:B6", "D10", |room| {
        room.settle();
        room.run(1, "home.insertRows", json!({"rows": "2:2"})).unwrap();
    });
    for i in 0..2 {
        assert_eq!(cell_on(&room, i, 0, "A6").0, Value::Number(5.0), "peer {i}: the block stays where the insert put it");
        assert_eq!(cell_on(&room, i, 0, "B7").0, Value::Number(60.0), "peer {i}");
    }
}

#[test]
fn a_cut_held_across_a_peers_column_delete_is_cancelled() {
    let room = cut_cancelled_by("B5:C6", "E10", |room| {
        room.settle();
        room.run(1, "home.deleteColumns", json!({"cols": "A:A"})).unwrap();
    });
    for i in 0..2 {
        assert_eq!(cell_on(&room, i, 0, "A5").0, Value::Number(50.0), "peer {i}: the block stays where the delete put it");
    }
}

#[test]
fn a_cut_held_across_a_checkpoint_that_replaces_the_workbook_is_cancelled() {
    // The room's checkpoint replaces alice's replica: the workbook is rebuilt from the room's
    // state, so the cut's stored range may name other cells now.
    cut_cancelled_by("A5:B6", "D10", |room| {
        room.settle();
        let state = room.replica_state(1);
        let p = &mut room.peers[0];
        p.live.apply_checkpoint(&mut p.session, &state).unwrap();
    });
}

#[test]
fn a_peers_write_into_the_cut_block_cancels_the_cut_and_keeps_their_value() {
    // GC1 M3: bob typed into the block after alice's cut. The move would wipe his 7 at A5
    // without a word, so the cut is cancelled instead.
    let room = cut_cancelled_by("A5:B6", "D10", |room| {
        room.settle();
        room.run(1, "cell.set", json!({"cell": "A5", "input": "7"})).unwrap();
    });
    for i in 0..2 {
        assert_eq!(cell_on(&room, i, 0, "A5").0, Value::Number(7.0), "peer {i}: bob's value is kept");
        assert_eq!(cell_on(&room, i, 0, "D10"), (Value::Empty, None), "peer {i}: nothing reached the target");
    }
}

#[test]
fn a_peers_write_beside_the_used_part_of_a_whole_row_cut_cancels_it() {
    // GC1 fix round 1, M1: a cut of whole rows keeps only their used part as its range, but the
    // paste moves the whole rows. Bob's F5 is in the rows the move takes, so the cut ends.
    let room = cut_cancelled_by("5:6", "A20", |room| {
        room.settle();
        room.run(1, "cell.set", json!({"cell": "F5", "input": "9"})).unwrap();
    });
    for i in 0..2 {
        assert_eq!(cell_on(&room, i, 0, "F5").0, Value::Number(9.0), "peer {i}: bob's value stays where he typed it");
        assert_eq!(cell_on(&room, i, 0, "F20"), (Value::Empty, None), "peer {i}: nothing moved");
    }
}

#[test]
fn a_peers_write_outside_the_cut_block_leaves_the_cut_alone() {
    let mut room = Room::new(2);
    room.run(0, "edit.cut", json!({"range": "A5:A6"})).unwrap();
    room.settle();
    room.run(1, "cell.set", json!({"cell": "A7", "input": "70"})).unwrap();
    room.settle();
    room.select(0, "D10");
    room.run(0, "edit.paste", json!({})).unwrap();
    room.settle();
    assert!(!told_cut_again(&room, 0), "{:?}", room.notices_of(0));
    same_everywhere(&room, "after the move");
    for i in 0..2 {
        assert_eq!(cell_on(&room, i, 0, "D10").0, Value::Number(5.0), "peer {i}");
        assert_eq!(cell_on(&room, i, 0, "A7").0, Value::Number(70.0), "peer {i}");
    }
}

/// The base with a print area, a defined name and a chart, none of them on the block A7:A8.
fn book_with_names_and_a_chart() -> Vec<u8> {
    let (wb, _) = io::open_bytes(FILE, &base_bytes(false)).unwrap();
    let mut s = Session::new();
    s.add_document(DocState::new(wb, None, "names".to_owned(), Trust::FromFile));
    s.execute("formulas.defineName", json!({"name": "Rate", "refersTo": "=Sheet1!$C$1"})).unwrap();
    s.execute("pageLayout.printArea", json!({"range": "A1:C5"})).unwrap();
    s.execute("insert.chart", json!({"range": "A1:A4", "type": "column", "at": "H2"})).unwrap();
    let wb = &s.active().unwrap().wb;
    assert!(wb.names.iter().any(|n| n.name == "Rate"), "{:?}", wb.names);
    assert!(wb.sheet(0).is_some_and(|s| s.print.print_area.is_some()), "the print area is set");
    assert_eq!(wb.sheet(0).map(|s| s.charts.len()), Some(1));
    io::save_bytes(wb, FILE).unwrap()
}

#[test]
fn a_book_with_a_print_area_a_name_and_a_chart_still_cuts_and_pastes_live() {
    // GX1 review: names and chart references are printed again on every cut. When that text
    // comes out as it was, the move is plain cells and is not refused.
    let bytes = book_with_names_and_a_chart();
    let mut room = Room::with(2, bytes.clone(), None);
    let names_before = room.wb(0).names.clone();
    let area_before = room.wb(0).sheet(0).and_then(|s| s.print.print_area);
    assert!(area_before.is_some(), "the file keeps its print area");
    let steps: Vec<(&str, Json)> =
        vec![("edit.cut", json!({"range": "A7:A8"})), ("selection.set", json!({"range": "E10"})), ("edit.paste", json!({}))];
    run_all(&mut room, 0, &steps);
    room.settle();
    assert!(room.notices_of(0).is_empty(), "the move was not refused: {:?}", room.notices_of(0));
    let (wb, _) = io::open_bytes(FILE, &bytes).unwrap();
    let mut s = Session::new();
    s.add_document(DocState::new(wb, None, "desktop".to_owned(), Trust::FromFile));
    for (id, p) in &steps {
        s.execute(id, p.clone()).unwrap();
    }
    let want = finger(&s.active().unwrap().wb);
    assert_eq!(room.finger(0), want, "the live move is the desktop move:\n{}", diff(&want, &room.finger(0)));
    same_everywhere(&room, "after the move");
    for i in 0..2 {
        assert_eq!(cell_on(&room, i, 0, "E11").0, Value::Number(8.0), "peer {i}: the block moved");
        assert_eq!(room.wb(i).names, names_before, "peer {i}: the names are as they were");
        assert_eq!(room.wb(i).sheet(0).and_then(|s| s.print.print_area), area_before, "peer {i}: the print area is as it was");
        assert_eq!(room.wb(i).sheet(0).map(|s| s.charts.len()), Some(1), "peer {i}: the chart is kept");
    }
}

/// Alice moves A1:B2 to D5 (G1 follows it).
fn moved() -> (Room, String) {
    let mut room = Room::new(2);
    room.run(0, "cell.set", json!({"cell": "G1", "input": "=A2+B2"})).unwrap();
    room.settle();
    let before = room.finger(0);
    run_all(&mut room, 0, &[("edit.cut", json!({"range": "A1:B2"})), ("selection.set", json!({"range": "D5"})), ("edit.paste", json!({}))]);
    room.settle();
    assert_eq!(cell_on(&room, 0, 0, "D6").0, Value::Number(2.0));
    (room, before)
}

#[test]
fn another_peers_undo_leaves_the_move_alone() {
    let (mut room, _) = moved();
    room.run(1, "cell.set", json!({"cell": "J1", "input": "bob"})).unwrap();
    room.settle();
    let after_move = cell_on(&room, 1, 0, "D6");
    room.run(1, "edit.undo", json!({})).unwrap();
    room.settle();
    same_everywhere(&room, "after bob's undo");
    for i in 0..2 {
        assert_eq!(cell_on(&room, i, 0, "J1"), (Value::Empty, None), "peer {i}: bob's own edit is undone");
        assert_eq!(cell_on(&room, i, 0, "D6"), after_move, "peer {i}: alice's move stays");
        assert_eq!(cell_on(&room, i, 0, "A2"), (Value::Empty, None), "peer {i}: the source stays empty");
        assert_eq!(cell_on(&room, i, 0, "G1").1.as_deref(), Some("D6+E6"), "peer {i}");
    }
}

#[test]
fn redo_moves_the_block_again() {
    let (mut room, before) = moved();
    let after = room.finger(0);
    room.run(0, "edit.undo", json!({})).unwrap();
    room.settle();
    assert_eq!(room.finger(0), before, "undo puts the block back:\n{}", diff(&before, &room.finger(0)));
    room.run(0, "edit.redo", json!({})).unwrap();
    room.settle();
    assert_eq!(room.finger(0), after, "redo moves it again:\n{}", diff(&after, &room.finger(0)));
    same_everywhere(&room, "after the redo");
}

#[test]
fn undo_after_a_peer_edited_a_moved_cell_keeps_their_value_and_says_so() {
    let (mut room, before) = moved();
    room.run(1, "cell.set", json!({"cell": "D6", "input": "99"})).unwrap();
    room.settle();
    room.run(0, "edit.undo", json!({})).unwrap();
    room.settle();
    same_everywhere(&room, "after the undo");
    for i in 0..2 {
        assert_eq!(cell_on(&room, i, 0, "D6").0, Value::Number(99.0), "peer {i}: bob's value is kept");
        assert_eq!(cell_on(&room, i, 0, "B2").0, Value::Number(42.0), "peer {i}: the rest of the block went back");
        assert_eq!(cell_on(&room, i, 0, "E6"), (Value::Empty, None), "peer {i}");
    }
    assert!(!room.notices_of(0).is_empty(), "alice is told part of the undo was skipped");
    assert_ne!(room.finger(0), before, "the undo did not take bob's edit back");
}

#[test]
fn a_formula_on_another_sheet_follows_the_move() {
    let mut room = Room::new(2);
    room.run(0, "sheet.activate", json!({"sheet": 1})).unwrap();
    room.run(0, "cell.set", json!({"cell": "A1", "input": "=Sheet1!A2*2"})).unwrap();
    room.run(0, "sheet.activate", json!({"sheet": 0})).unwrap();
    room.settle();
    run_all(&mut room, 0, &[("edit.cut", json!({"range": "A1:A2"})), ("selection.set", json!({"range": "D5"})), ("edit.paste", json!({}))]);
    room.settle();
    same_everywhere(&room, "after the move");
    for i in 0..2 {
        assert_eq!(cell_on(&room, i, 1, "A1").1.as_deref(), Some("Sheet1!D6*2"), "peer {i}");
        assert_eq!(cell_on(&room, i, 1, "A1").0, Value::Number(4.0), "peer {i}");
    }
}
