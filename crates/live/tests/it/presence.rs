//! Presence: carets in slot ids, followed through row inserts and deletes.

use craft_live::{Access, SERVICE_UID};
use gridcraft_core::{CellRef, RangeRef, Value};
use gridcraft_live::{GridPos, Id, Layout, presence};
use serde_json::{Value as Json, json};

use crate::support::{Room, a1};

/// Peer `i` tells the room where its selection is.
fn send(room: &mut Room, i: usize) {
    let p = &mut room.peers[i];
    p.live.send_selection(&p.session);
}

/// Where peer `of`'s caret is drawn for peer `at`, after one more pump of `at`.
fn caret(room: &mut Room, at: usize, of: &str) -> Option<GridPos> {
    let p = &mut room.peers[at];
    let peers = p.live.pump(&mut p.session);
    peers.into_iter().find(|x| x.display == of).and_then(|x| x.caret)
}

fn number_at(room: &Room, i: usize, at: CellRef) -> Value {
    room.wb(i).sheet(0).and_then(|s| s.cell(at)).map(|c| c.value.clone()).unwrap_or(Value::Empty)
}

#[test]
fn cursor_payload_round_trips_and_stays_under_16_kib() {
    let room = Room::new(1);
    let layout = Layout::of_base(&room.base());
    let active = a1("C7");
    let few = [RangeRef::new(a1("A1"), a1("B2")), RangeRef::new(a1("D4"), a1("E9"))];
    let p = presence::payload(&layout, 0, active, &few, true).unwrap();
    assert_eq!(p["e"], json!(1));
    assert_eq!(p["o"], Json::Null);
    assert_eq!(p["r"].as_array().unwrap().len(), 2);
    let mut c = presence::parse(&p).unwrap();
    assert!(presence::resolve(&layout, &mut c));
    let at = c.at().unwrap();
    assert_eq!((at.active, at.ranges.clone(), at.editing), (active, few.to_vec(), true), "ids resolve back to the cells sent");

    let many: Vec<RangeRef> = (0..1000u32).map(|i| RangeRef::new(CellRef::new(i * 2, i % 40), CellRef::new(i * 2 + 1, i % 40 + 3))).collect();
    let p = presence::payload(&layout, 0, active, &many, false).unwrap();
    let sent = p["r"].as_array().unwrap().len();
    assert!((1..=33).contains(&sent), "{sent} ranges sent");
    assert!(serde_json::to_string(&p).unwrap().len() < 16 * 1024);
    let mut c = presence::parse(&p).unwrap();
    assert!(presence::resolve(&layout, &mut c));
    let boxed = c.at().unwrap().ranges.clone();
    for r in &many {
        let covered = boxed.iter().any(|b| {
            let (r0, r1, c0, c1) = (b.start.row.min(b.end.row), b.start.row.max(b.end.row), b.start.col.min(b.end.col), b.start.col.max(b.end.col));
            [r.start, r.end].iter().all(|x| (r0..=r1).contains(&x.row) && (c0..=c1).contains(&x.col))
        });
        assert!(covered, "{r:?} lies inside what was sent");
    }
}

#[test]
fn cursor_follows_a_remote_row_insert() {
    let mut room = Room::new(2);
    // Alice is on row 5 (the base file holds 5 in A5).
    room.select(0, "A5");
    send(&mut room, 0);
    room.settle();
    let at = caret(&mut room, 1, "Alice").expect("Bob sees Alice's caret");
    assert_eq!(at.active, a1("A5"));
    assert_eq!(number_at(&room, 1, at.active), Value::Number(5.0));

    // Bob inserts three rows above it: her caret is on the same row content, shown at row 8.
    room.run(1, "home.insertRows", json!({"rows": "2:4"})).unwrap();
    room.settle();
    let at = caret(&mut room, 1, "Alice").expect("the caret is still there");
    assert_eq!(at.active, a1("A8"), "the caret moved with its row");
    assert_eq!(number_at(&room, 1, at.active), Value::Number(5.0), "and stands on the same row content");

    // Bob deletes the row she is on: the caret stays, on the row that moved into its place.
    room.run(1, "home.deleteRows", json!({"rows": "8:8"})).unwrap();
    room.settle();
    let at = caret(&mut room, 1, "Alice").expect("a caret on a deleted row degrades, it is not dropped");
    assert!(!at.unknown);
    assert_eq!(at.active, a1("A8"));
    assert_eq!(number_at(&room, 1, at.active), Value::Number(6.0), "it shows on the row that took the deleted row's place");
}

#[test]
fn cursor_on_an_unknown_id_is_hidden_then_shown() {
    let mut room = Room::new(2);
    room.run(0, "home.insertRows", json!({"rows": "3:3"})).unwrap();
    room.settle();
    let layout = Layout::from_doc(room.peers[0].live.replica().unwrap(), &room.base());
    let col = layout.sheets[0].cols.id_at(0).unwrap();
    // The id Alice's second insert mints, read from an identical room that has run both inserts
    // (ids are deterministic per join order), so the test does not assume how ids are numbered.
    let next = {
        let mut twin = Room::new(2);
        twin.run(0, "home.insertRows", json!({"rows": "3:3"})).unwrap();
        twin.settle();
        twin.run(0, "home.insertRows", json!({"rows": "6:6"})).unwrap();
        twin.settle();
        Layout::from_doc(twin.peers[0].live.replica().unwrap(), &twin.base()).sheets[0].rows.id_at(5).unwrap()
    };
    assert_ne!(layout.sheets[0].rows.id_at(5), Some(next), "the id is not known yet");
    let forged = json!({"s": Id::base(0).text(), "a": [next.text(), col.text()], "r": [], "e": 0, "o": null});
    room.peers[0].live.core_mut().send_presence(forged);
    room.settle();
    let at = caret(&mut room, 1, "Alice").expect("the peer is there with a caret entry");
    assert!(at.unknown, "an id this window does not know draws nothing");

    room.run(0, "home.insertRows", json!({"rows": "6:6"})).unwrap();
    room.settle();
    let now = Layout::from_doc(room.peers[1].live.replica().unwrap(), &room.base());
    assert_eq!(now.sheets[0].rows.id_at(5), Some(next), "the second insert minted the id the caret named");
    let at = caret(&mut room, 1, "Alice").expect("shown once the id is known");
    assert!(!at.unknown);
    assert_eq!(at.active, a1("A6"));
}

#[test]
fn roster_leaves_out_the_service_peer() {
    let mut room = Room::new(2);
    let _service = room.relay.connect(SERVICE_UID, "Document service", true);
    room.settle();
    let p = &mut room.peers[0];
    let names: Vec<String> = p.live.pump(&mut p.session).into_iter().map(|x| x.display).collect();
    assert_eq!(names, vec!["Bob".to_owned()], "only the other editor, not the saver");
}

#[test]
fn read_only_peer_still_sends_its_cursor() {
    let mut room = Room::new(2);
    let viewer = room.join("Zed", Access::Read, None);
    room.settle();
    room.select(viewer, "C3");
    send(&mut room, viewer);
    room.settle();
    let at = caret(&mut room, 0, "Zed").expect("the viewer's caret reaches the editors");
    assert_eq!(at.active, a1("C3"));
}

#[test]
fn malformed_payloads_are_refused() {
    let ok = json!({"s": "1", "a": ["1", "1"], "r": [["1", "1", "2", "2"]], "e": 0, "o": null});
    assert!(presence::parse(&ok).is_some());
    let long = "z".repeat(14);
    let huge_text = "z".repeat(10_000_000);
    let many: Vec<Json> = (0..100_000).map(|_| json!(["1", "1", "1", "1"])).collect();
    let thirty_three: Vec<Json> = (0..33).map(|_| json!(["1", "1", "1", "1"])).collect();
    let mut cases: Vec<(&str, Json)> = vec![
        ("not an object", json!([1, 2])),
        ("no sheet", json!({"a": ["1", "1"]})),
        ("empty sheet", json!({"s": "", "a": ["1", "1"]})),
        ("sheet is a number", json!({"s": 1, "a": ["1", "1"]})),
        ("a has one id", json!({"s": "1", "a": ["1"]})),
        ("a has three ids", json!({"s": "1", "a": ["1", "1", "1"]})),
        ("a is not an array", json!({"s": "1", "a": "1.1"})),
        ("a holds numbers", json!({"s": "1", "a": [1, 1]})),
        ("id overflows u64", json!({"s": "1", "a": ["zzzzzzzzzzzzz", "1"]})),
        ("id with a capital", json!({"s": "1", "a": ["A", "1"]})),
        ("id too long", json!({"s": long, "a": ["1", "1"]})),
        ("r is not an array", json!({"s": "1", "a": ["1", "1"], "r": "x"})),
        ("range of three", json!({"s": "1", "a": ["1", "1"], "r": [["1", "1", "1"]]})),
        ("range of non-ids", json!({"s": "1", "a": ["1", "1"], "r": [[1, 1, 1, 1]]})),
        ("33 ranges", json!({"s": "1", "a": ["1", "1"], "r": thirty_three})),
        ("10 MB id text", json!({"s": huge_text, "a": ["1", "1"]})),
    ];
    cases.push(("100000 ranges", json!({"s": "1", "a": ["1", "1"], "r": many})));
    for (what, p) in &cases {
        assert!(presence::parse(p).is_none(), "{what}: refused");
    }
    // `e` that is not 0 or 1 reads as not editing, and `o` is ignored.
    let c = presence::parse(&json!({"s": "1", "a": ["1", "1"], "e": "x", "o": {"any": 1}})).unwrap();
    assert!(!c.editing);
}

#[test]
fn a_hostile_cursor_changes_nothing_and_is_dropped() {
    let mut room = Room::new(2);
    let now = std::sync::Arc::new(std::sync::Mutex::new(0.0_f64));
    let read = now.clone();
    room.peers[0].live.set_clock(Box::new(move || *read.lock().unwrap()));
    room.select(0, "A5");
    send(&mut room, 0);
    room.settle();
    assert!(caret(&mut room, 1, "Alice").is_some());
    let before = room.finger(1);
    let big = "z".repeat(5_000_000);
    room.peers[0].live.core_mut().send_presence(json!({"s": big, "a": ["1", "1"], "r": [], "e": 0, "o": null}));
    // The link holds carets to five a second on its clock.
    *now.lock().unwrap() += 250.0;
    room.settle();
    let got = caret(&mut room, 1, "Alice");
    assert!(got.is_none(), "the hostile caret is dropped, still {got:?}");
    assert_eq!(room.finger(1), before, "no change to the workbook");
    room.assert_all_live("hostile cursor");
}

#[test]
#[allow(clippy::disallowed_methods)] // a native-only test: it times one frame
fn fifty_carets_resolve_cheaply_after_a_layout_change() {
    let mut room = Room::new(2);
    for i in 0..48 {
        room.join(&format!("Peer{i}"), Access::Edit, None);
    }
    room.settle();
    for i in 1..room.peers.len() {
        room.select(i, &format!("A{}", 1 + i % 8));
        send(&mut room, i);
    }
    room.settle();
    let n = room.peers.len();
    assert_eq!(n, 50);
    room.run(0, "home.insertRows", json!({"rows": "1:3"})).unwrap();
    room.settle();
    let p = &mut room.peers[0];
    let started = std::time::Instant::now();
    let peers = p.live.pump(&mut p.session);
    let took = started.elapsed();
    assert_eq!(peers.len(), n - 1);
    for x in &peers {
        let at = x.caret.as_ref().unwrap_or_else(|| panic!("{} has a caret", x.display));
        assert!(!at.unknown);
        assert!(at.active.row >= 3, "{}: below the three inserted rows, at {:?}", x.display, at.active);
    }
    assert!(took < std::time::Duration::from_millis(500), "one frame with 49 carets took {took:?}");
}
