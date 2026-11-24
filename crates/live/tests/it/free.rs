//! Free edits between peers: cells, formats, sizes and hidden flags, through an in-memory relay.

use craft_collab::Replica;
use craft_live::Access;
use gridcraft_core::{CellRef, Value};
use gridcraft_live::{CellContent, Entry, GridDoc, Id, NOT_YET_TEXT, cell_key, volatile_for, workbook_from_state};
use serde_json::{Value as Json, json};

use crate::support::{Room, a1, base_bytes, diff, finger};
use crate::{Rng, seeds};

fn col_letter(c: usize) -> char {
    (b'A' + u8::try_from(c).unwrap_or(0)) as char
}

fn some_cell(rng: &mut Rng) -> String {
    format!("{}{}", col_letter(rng.below(5)), 1 + rng.below(8))
}

fn some_range(rng: &mut Rng) -> String {
    let (r0, c0) = (rng.below(6), rng.below(4));
    format!("{}{}:{}{}", col_letter(c0), 1 + r0, col_letter(c0 + rng.below(2)), 1 + r0 + rng.below(3))
}

/// One random FREE command and its params.
fn command(rng: &mut Rng) -> (&'static str, Json) {
    let row = 1 + rng.below(8);
    match rng.below(12) {
        0 => ("cell.set", json!({"cell": some_cell(rng), "input": format!("{}", rng.below(1000))})),
        1 => ("cell.set", json!({"cell": some_cell(rng), "input": format!("=A{}+B{}", 1 + rng.below(8), 1 + rng.below(8))})),
        2 => ("cell.set", json!({"cell": some_cell(rng), "input": format!("text {}", rng.below(50))})),
        3 => ("range.setValues", json!({"range": "C3", "values": [[rng.below(99)], [rng.below(99)]]})),
        4 => ("edit.clearContents", json!({"range": some_range(rng)})),
        5 => ("home.bold", json!({"range": some_range(rng)})),
        6 => ("home.fillColor", json!({"range": some_range(rng), "color": format!("#{:06X}", rng.below(0xFF_FFFF))})),
        7 => ("home.rowHeight", json!({"rows": format!("{row}:{row}"), "height": 20 + rng.below(40)})),
        8 => ("home.columnWidth", json!({"cols": format!("{0}:{0}", col_letter(rng.below(5))), "width": 60 + rng.below(80)})),
        9 => ("home.hideRows", json!({"rows": format!("{row}:{row}")})),
        10 => ("home.unhideRows", json!({"rows": format!("{row}:{row}")})),
        _ => ("edit.fillDown", json!({"range": format!("E1:E{}", 2 + rng.below(4))})),
    }
}

fn converge(peers: usize, seed: u64) {
    let mut rng = Rng(seed ^ (peers as u64) << 32);
    let mut room = Room::new(peers);
    for step in 0..40 {
        let who = rng.below(peers);
        let (id, params) = command(&mut rng);
        if step % 9 == 0 {
            let text = (0..3).map(|r| format!("p{r}\tq{r}\t{}", rng.below(9))).collect::<Vec<_>>().join("\n");
            let at = some_cell(&mut rng);
            if let Err(e) = room.run(who, "edit.paste", json!({"at": at, "text": text})) {
                panic!("seed {seed}: paste failed unexpectedly: {e}");
            }
        } else if let Err(e) = room.run(who, id, params) {
            assert_ne!(e, NOT_YET_TEXT, "seed {seed}: {id} is a free command");
            panic!("seed {seed}: {id} failed unexpectedly: {e}");
        }
        // Some peers pump, in a drawn order: delivery is seeded.
        let mut order: Vec<usize> = (0..peers).collect();
        for i in (1..order.len()).rev() {
            order.swap(i, rng.below(i + 1));
        }
        for i in order.into_iter().filter(|_| rng.chance(50)) {
            room.pump(i);
        }
    }
    room.settle();
    room.assert_all_live(&format!("seed {seed}"));
    let first = room.finger(0);
    for i in 1..peers {
        let other = room.finger(i);
        assert!(first == other, "seed {seed}: peers 0 and {i} differ (peers {peers})\n{}", diff(&first, &other));
    }
    let state = room.replica_state(0);
    let base = room.base();
    let source = volatile_for(&base, &state).unwrap();
    let oracle = workbook_from_state(&base, &state, Some(source)).unwrap();
    let want = finger(&oracle);
    assert!(first == want, "seed {seed}: the peers differ from base plus state (peers {peers})\n{}", diff(&first, &want));
}

#[test]
fn two_peers_typing_converge() {
    for seed in seeds() {
        converge(2, seed);
    }
}

#[test]
fn eight_peers_typing_converge() {
    for seed in seeds().into_iter().take(6) {
        converge(8, seed);
    }
}

#[test]
fn same_cell_one_value_wins_everywhere_and_the_loser_is_told() {
    let mut room = Room::new(2);
    room.run(0, "cell.set", json!({"cell": "D5", "input": "from alice"})).unwrap();
    room.run(1, "cell.set", json!({"cell": "D5", "input": "from bob"})).unwrap();
    room.settle();
    assert_eq!(room.finger(0), room.finger(1));
    let shown = |i: usize| room.wb(i).sheet(0).unwrap().cell(a1("D5")).unwrap().value.clone();
    assert_eq!(shown(0), shown(1), "one value wins everywhere");
    let winner = if shown(0) == Value::Text("from alice".into()) { "Alice" } else { "Bob" };
    let loser = usize::from(winner == "Alice");
    assert!(room.notices_of(loser).iter().any(|n| n.contains(winner)), "the loser is told who won: {:?}", room.notices_of(loser));
    assert!(room.notices_of(1 - loser).iter().all(|n| !n.contains("also changed")), "the winner is not told");
}

#[test]
fn value_and_format_on_one_cell_both_kept() {
    let mut room = Room::new(2);
    room.run(0, "cell.set", json!({"cell": "D5", "input": "kept"})).unwrap();
    room.run(1, "home.bold", json!({"range": "D5"})).unwrap();
    room.settle();
    for i in 0..2 {
        let cell = room.wb(i).sheet(0).unwrap().cell(a1("D5")).unwrap().clone();
        assert_eq!(cell.value, Value::Text("kept".into()), "peer {i} keeps the value");
        assert!(room.wb(i).styles.get(cell.style).font.bold, "peer {i} keeps the bold");
    }
    assert_eq!(room.finger(0), room.finger(1));
}

#[test]
fn clearing_a_base_cell_survives_a_checkpoint_join() {
    let mut room = Room::new(2);
    assert!(room.wb(0).sheet(0).unwrap().cell(a1("B2")).is_some(), "the base has B2");
    room.run(0, "edit.clearContents", json!({"range": "B2"})).unwrap();
    room.settle();
    let sheet = Id::base(0);
    let key = cell_key(Id::base(1), Id::base(1));
    let state = room.replica_state(1);
    let joined = GridDoc::from_state(&state).unwrap();
    assert_eq!(joined.cell(sheet, &key), Entry::Cleared, "absent is not cleared, and the join keeps the difference");
    let wb = workbook_from_state(&room.base(), &state, None).unwrap();
    assert!(wb.sheet(0).unwrap().cell(a1("B2")).is_none_or(|c| !c.has_content()), "B2 is empty for a peer that joins from the state");
    // A peer that joins the running room by replay agrees.
    let late = room.join("Carol", Access::Edit, None);
    room.settle();
    assert_eq!(room.finger(0), room.finger(late));
    assert!(room.wb(late).sheet(0).unwrap().cell(a1("B2")).is_none_or(|c| !c.has_content()));
}

#[test]
fn a_typed_formula_is_shared_in_id_form() {
    let mut room = Room::new(2);
    room.run(0, "cell.set", json!({"cell": "D4", "input": "=B2+A3"})).unwrap();
    room.settle();
    let key = cell_key(Id::base(3), Id::base(3));
    let Entry::Set(c) = room.peers[1].live.replica().unwrap().cell(Id::base(0), &key) else { panic!("no content") };
    assert!(c.f.as_deref().unwrap().contains("@{"), "the replica holds id form: {:?}", c.f);
    assert_eq!(c.u.as_deref(), Some("alice"), "the author's uid travels with the cell");
    let text = |i: usize| room.wb(i).sheet(0).unwrap().cell(a1("D4")).unwrap().formula.as_ref().unwrap().text.clone();
    assert_eq!(text(0), text(1));
    assert_eq!(room.wb(0).sheet(0).unwrap().cell(a1("D4")).unwrap().value, room.wb(1).sheet(0).unwrap().cell(a1("D4")).unwrap().value);
}

#[test]
fn two_peers_creating_the_same_style_write_the_same_key() {
    let mut room = Room::new(2);
    room.run(0, "home.fillColor", json!({"range": "E1", "color": "#123456"})).unwrap();
    room.run(1, "home.fillColor", json!({"range": "E2", "color": "#123456"})).unwrap();
    room.settle();
    let doc = room.peers[0].live.replica().unwrap();
    let fmt = |r: u32| match doc.fmt(Id::base(0), &cell_key(Id::base(r), Id::base(4))) {
        Entry::Set(k) => k,
        other => panic!("no style key: {other:?}"),
    };
    assert_eq!(fmt(0), fmt(1), "equal styles get one key");
    assert!(doc.style(&fmt(0)).is_some());
    assert_eq!(room.finger(0), room.finger(1));
}

fn one_key(_: &[u8]) -> u64 {
    7
}

#[test]
fn a_style_key_collision_falls_back_to_a_minted_key() {
    let mut room = Room::with(2, base_bytes(false), Some(one_key));
    room.run(0, "home.fillColor", json!({"range": "E1", "color": "#FF0000"})).unwrap();
    room.settle();
    room.run(1, "home.fillColor", json!({"range": "E2", "color": "#0000FF"})).unwrap();
    room.settle();
    let doc = room.peers[0].live.replica().unwrap();
    let key = |r: u32| match doc.fmt(Id::base(0), &cell_key(Id::base(r), Id::base(4))) {
        Entry::Set(k) => k,
        other => panic!("no style key: {other:?}"),
    };
    assert_ne!(key(0), key(1), "the second style did not overwrite the first");
    assert!(key(1).starts_with('m'), "the second key is minted: {}", key(1));
    assert_ne!(doc.style(&key(0)), doc.style(&key(1)), "both styles survive and differ");
    let fill = |i: usize, c: &str| room.wb(i).styles.get(room.wb(i).sheet(0).unwrap().cell(a1(c)).unwrap().style).fill;
    assert_ne!(fill(0, "E1"), fill(0, "E2"));
    assert_eq!(room.finger(0), room.finger(1));
}

#[test]
fn concurrent_hide_and_resize_keep_both() {
    let mut room = Room::new(2);
    room.run(0, "home.hideRows", json!({"rows": "5:5"})).unwrap();
    room.run(1, "home.rowHeight", json!({"rows": "5:5", "height": 55})).unwrap();
    room.settle();
    for i in 0..2 {
        let info = room.wb(i).sheet(0).unwrap().rows.get(&4).copied().unwrap();
        assert!(info.hidden, "peer {i}: still hidden");
        assert!(info.size.is_some_and(|s| s > 40.0), "peer {i}: resized, {:?}", info.size);
    }
    assert_eq!(room.finger(0), room.finger(1));
}

#[test]
fn manual_unhide_does_not_reveal_a_filter_hidden_row() {
    let mut room = Room::with(2, base_bytes(true), None);
    let hidden = |r: &Room, i: usize| r.wb(i).sheet(0).unwrap().rows.get(&2).is_some_and(|l| l.hidden);
    assert!(hidden(&room, 0) && hidden(&room, 1), "the base row is hidden by its filter");
    room.run(0, "home.unhideRows", json!({"rows": "3:3"})).unwrap();
    room.settle();
    assert!(hidden(&room, 0), "the author's own view shows the row hidden again");
    assert!(hidden(&room, 1), "the other peer never saw it revealed");
    assert_eq!(room.finger(0), room.finger(1));
}

#[test]
fn a_paste_of_10k_cells_converges_in_parts() {
    let mut room = Room::new(2);
    let long = "x".repeat(180);
    let text = (0..100).map(|r| (0..100).map(|c| format!("{long}{r}-{c}")).collect::<Vec<_>>().join("\t")).collect::<Vec<_>>().join("\n");
    let before = room.peers[0].sizes.borrow().len();
    room.run(0, "edit.paste", json!({"at": "A20", "text": text})).unwrap();
    room.settle();
    let sizes: Vec<usize> = room.peers[0].sizes.borrow()[before..].to_vec();
    let cmds = room.peers[0].live.take_commands();
    let paste: Vec<_> = cmds.iter().filter(|c| c.updates > 1).collect();
    assert_eq!(paste.len(), 1, "the paste is one command: {cmds:?}");
    assert_eq!(paste[0].updates, sizes.len(), "whose parts are all its updates");
    assert!(sizes.len() > 1, "the paste went in parts: {sizes:?} notices {:?}", room.notices_of(0));
    assert!(sizes.iter().all(|s| *s <= gridcraft_live::split::UPDATE_MAX_BYTES), "every update is within the cap: {sizes:?}");
    assert_eq!(room.finger(0), room.finger(1));
    assert!(room.wb(1).sheet(0).unwrap().cell(CellRef::new(118, 99)).is_some(), "the last cell arrived");
}

#[test]
fn replace_all_while_another_types_keeps_both_per_cell() {
    let mut room = Room::new(2);
    room.run(0, "edit.replace", json!({"what": "xx", "with": "yy"})).unwrap();
    room.run(1, "cell.set", json!({"cell": "F2", "input": "typed meanwhile"})).unwrap();
    room.settle();
    for i in 0..2 {
        let sheet = room.wb(i).sheet(0).unwrap();
        for r in 0..8u32 {
            assert_eq!(sheet.cell(CellRef::new(r, 2)).unwrap().value, Value::Text(format!("yy {r}").into()), "peer {i} row {r}");
        }
        assert_eq!(sheet.cell(a1("F2")).unwrap().value, Value::Text("typed meanwhile".into()));
    }
    assert_eq!(room.finger(0), room.finger(1));
}

#[test]
fn every_kind_of_value_survives_the_shared_encoding() {
    use gridcraft_core::CellError;
    use gridcraft_live::cells::{decode_value, encode_value};
    let samples = [
        Value::Number(0.0),
        Value::Number(-0.0),
        Value::Number(1.0 / 3.0),
        Value::Number(1e300),
        Value::Number(9_007_199_254_740_993.0),
        Value::Text("".into()),
        Value::Text("tab\tand \"quotes\" \u{1F600}".into()),
        Value::Bool(true),
        Value::Bool(false),
        Value::Error(CellError::Div0),
        Value::Error(CellError::NA),
    ];
    for v in samples {
        let shared = encode_value(&v);
        // Through a replica too, not only through JSON.
        let mut doc = GridDoc::seeded(&gridcraft_live::GridSeed { base_sha256: [1; 32], sheets: 1 }).unwrap();
        doc.set_client(1).unwrap();
        let content = CellContent { v: shared.clone(), ..CellContent::default() };
        doc.write(|t| t.set_cell(Id::base(0), "k", &content)).unwrap();
        let Entry::Set(back) = doc.cell(Id::base(0), "k") else { panic!("not stored") };
        assert_eq!(decode_value(back.v.as_ref()), v, "{v:?} round trips");
    }
    assert_eq!(encode_value(&Value::Empty), None, "empty has no value");
    assert_eq!(decode_value(None), Value::Empty);
    assert_eq!(decode_value(Some(&Json::Null)), Value::Empty, "null is empty, never a value");
}

#[test]
fn random_numbers_agree_on_every_peer_and_the_state() {
    let mut room = Room::new(3);
    room.run(0, "cell.set", json!({"cell": "G1", "input": "=RAND()"})).unwrap();
    room.run(1, "cell.set", json!({"cell": "G2", "input": "=RANDBETWEEN(1,1000000)"})).unwrap();
    room.run(2, "cell.set", json!({"cell": "H1", "input": "plain"})).unwrap();
    room.settle();
    assert_eq!(room.finger(0), room.finger(1));
    assert_eq!(room.finger(0), room.finger(2));
    let state = room.replica_state(0);
    let base = room.base();
    let oracle = workbook_from_state(&base, &state, Some(volatile_for(&base, &state).unwrap())).unwrap();
    assert_eq!(room.finger(0), finger(&oracle), "the saved workbook shows the same random numbers");
    fn draw(room: &Room, i: usize) -> Value {
        room.wb(i).sheet(0).unwrap().cell(a1("G1")).unwrap().value.clone()
    }
    let before = draw(&room, 0);
    room.run(1, "cell.set", json!({"cell": "H2", "input": "another edit"})).unwrap();
    room.settle();
    assert_eq!(draw(&room, 0), draw(&room, 2), "still equal after another edit");
    assert_ne!(before, draw(&room, 0), "every edit changes the numbers, as in a spreadsheet");
}

#[test]
fn a_later_overwrite_is_not_told_as_lost_but_a_concurrent_one_is() {
    // Sequential: Alice types, everyone settles, then Bob overwrites.
    let mut room = Room::new(2);
    room.run(0, "cell.set", json!({"cell": "D5", "input": "first"})).unwrap();
    room.settle();
    room.run(1, "cell.set", json!({"cell": "D5", "input": "second"})).unwrap();
    room.settle();
    room.run(0, "cell.set", json!({"cell": "D5", "input": "third"})).unwrap();
    room.settle();
    for i in 0..2 {
        assert!(room.notices_of(i).iter().all(|n| !n.contains("also changed")), "peer {i}: {:?}", room.notices_of(i));
    }
    assert_eq!(room.finger(0), room.finger(1));
    // Concurrent: both type before either hears the other.
    room.run(0, "cell.set", json!({"cell": "E5", "input": "alice"})).unwrap();
    room.run(1, "cell.set", json!({"cell": "E5", "input": "bob"})).unwrap();
    room.settle();
    let told = (0..2).filter(|i| room.notices_of(*i).iter().any(|n| n.contains("also changed"))).count();
    assert_eq!(told, 1, "exactly the loser is told");
}

#[test]
fn whole_column_and_row_formats_converge() {
    let mut room = Room::new(3);
    room.run(0, "home.bold", json!({"range": "C:C"})).unwrap();
    room.run(1, "cell.set", json!({"cell": "C3", "input": "typed in the column"})).unwrap();
    room.run(2, "home.fillColor", json!({"range": "6:6", "color": "#00FF00"})).unwrap();
    room.settle();
    room.assert_all_live("whole line formats");
    for i in 0..3 {
        let sheet = room.wb(i).sheet(0).unwrap();
        let col = sheet.cols.get(&2).copied().unwrap_or_default();
        assert!(col.style.is_some_and(|s| room.wb(i).styles.get(s).font.bold), "peer {i}: column C is bold");
        let row = sheet.rows.get(&5).copied().unwrap_or_default();
        assert!(row.style.is_some(), "peer {i}: row 6 has a fill; rows {:?} cols {:?}", sheet.rows, sheet.cols);
        assert_eq!(sheet.cell(a1("C3")).unwrap().value, Value::Text("typed in the column".into()), "peer {i}");
    }
    let (f0, f1, f2) = (room.finger(0), room.finger(1), room.finger(2));
    assert!(f0 == f1, "{}", diff(&f0, &f1));
    assert!(f0 == f2, "{}", diff(&f0, &f2));
    // And the saved state shows the same.
    let state = room.replica_state(0);
    let base = room.base();
    let oracle = workbook_from_state(&base, &state, None).unwrap();
    assert_eq!(room.finger(0), finger(&oracle));
    // A late joiner agrees.
    let late = room.join("Dana", Access::Edit, None);
    room.settle();
    assert_eq!(room.finger(0), room.finger(late));
}

#[test]
fn a_whole_line_format_turned_off_again_keeps_the_line_style() {
    // Bold on, then off, on column A leaves a column style that looks like the default, and a
    // row's style that looks like the default still hides a bold column under it. Every peer and
    // the saved state keep the line style, not "no style".
    let mut room = Room::new(2);
    room.run(0, "home.bold", json!({"range": "A:A"})).unwrap();
    room.run(0, "home.bold", json!({"range": "A:A"})).unwrap();
    room.run(1, "home.bold", json!({"range": "C:C"})).unwrap();
    room.run(1, "home.bold", json!({"range": "4:4"})).unwrap();
    room.run(1, "home.bold", json!({"range": "4:4"})).unwrap();
    room.settle();
    room.assert_all_live("a whole line format turned off");
    let (f0, f1) = (room.finger(0), room.finger(1));
    assert!(f0 == f1, "{}", diff(&f0, &f1));
    let base = room.base();
    for i in 0..2 {
        let oracle = workbook_from_state(&base, &room.replica_state(i), None).unwrap();
        assert!(room.finger(i) == finger(&oracle), "peer {i} differs from the state:\n{}", diff(&room.finger(i), &finger(&oracle)));
    }
}
