//! The grid document: the seed, absent against cleared, what a remote update names, hostile
//! bytes, and replicas that reach the same document whatever order their updates arrive in.

use craft_collab::Replica;
use gridcraft_live::{AxisKind, CellContent, Entry, GridChange, GridDoc, GridSeed, Id, Op, cell_key};
use serde_json::{Value, json};

use crate::{Rng, seeds};

fn seed(byte: u8) -> GridSeed {
    GridSeed { base_sha256: [byte; 32], sheets: 2 }
}

fn peer(s: &GridSeed, space: u32) -> GridDoc {
    let mut d = GridDoc::seeded(s).unwrap();
    d.set_client(space).unwrap();
    d
}

fn content(v: Value, by: &str) -> CellContent {
    CellContent { v: Some(v), u: Some(by.into()), ..CellContent::default() }
}

#[test]
fn seed_is_deterministic() {
    let s = seed(7);
    let (a, b) = (GridDoc::seeded(&s).unwrap(), GridDoc::seeded(&s).unwrap());
    assert_eq!(a.state(), b.state(), "equal seeds give equal state bytes");
    assert_eq!(a.meta(), b.meta());
    assert_ne!(a.state(), GridDoc::seeded(&seed(8)).unwrap().state(), "a different base gives a different seed");
}

#[test]
fn absent_and_cleared_differ_and_survive_a_join() {
    let s = seed(1);
    let mut d = peer(&s, 1);
    let sheet = Id::base(0);
    let keys: Vec<String> = (1..=4).map(|r| cell_key(Id::base(r), Id::base(1))).collect();
    d.write(|t| {
        t.set_cell(sheet, &keys[0], &content(json!(5), "alice"));
        t.clear_cell(sheet, &keys[1]);
        t.set_cell(sheet, &keys[2], &content(json!("x"), "alice"));
        t.reset_cell(sheet, &keys[2]);
        t.set_fmt(sheet, &keys[0], "s1");
        t.clear_fmt(sheet, &keys[1]);
    })
    .unwrap();
    let read = |d: &GridDoc| keys.iter().map(|k| (d.cell(sheet, k), d.fmt(sheet, k))).collect::<Vec<_>>();
    let here = read(&d);
    assert!(matches!(here[0].0, Entry::Set(_)));
    assert_eq!(here[1].0, Entry::Cleared, "a clear reads as cleared");
    assert_eq!(here[2].0, Entry::Absent, "a reset reads as the base's");
    assert_eq!(here[3].0, Entry::Absent, "an untouched key reads as the base's");
    assert_ne!(here[1].0, here[3].0, "cleared and absent differ");
    assert_ne!(here[1].1, here[3].1, "for formats too");
    let joined = GridDoc::from_state(&d.state()).unwrap();
    assert_eq!(read(&joined), here, "a joiner from the state reads the same");
}

#[test]
fn remote_changes_name_what_moved() {
    let s = seed(2);
    let (mut a, mut b) = (peer(&s, 1), peer(&s, 2));
    let sheet = Id::base(1);
    let key = cell_key(Id::base(3), Id::base(2));
    let u = a.write(|t| t.set_cell(sheet, &key, &content(json!(1), "alice"))).unwrap();
    assert!(b.apply(&u).unwrap().contains(&GridChange::Cells { sheet, keys: vec![key.clone()] }));
    let u = a.write(|t| t.set_fmt(sheet, &key, "s9")).unwrap();
    assert!(b.apply(&u).unwrap().contains(&GridChange::Formats { sheet, keys: vec![key.clone()] }));
    let line = format!("{}/h", Id::base(3).text());
    let u = a.write(|t| t.set_line(sheet, AxisKind::Rows, &line, json!(30))).unwrap();
    assert!(b.apply(&u).unwrap().contains(&GridChange::Lines { sheet, axis: AxisKind::Rows, keys: vec![line] }));

    let from = b.ops_len();
    let mut axis = a.axis(sheet, AxisKind::Cols, 100);
    let ids = [Id::minted(1, 1)];
    let ins = axis.insert(4, &ids);
    let op = Op::Insert { id: 1, by: "alice".into(), sheet, axis: AxisKind::Cols, prev: ins.prev, ids: ids.to_vec() };
    let u = a
        .write(|t| {
            t.axis_insert(sheet, AxisKind::Cols, &ins, &ids);
            t.push_op(&op);
        })
        .unwrap();
    let changes = b.apply(&u).unwrap();
    assert!(changes.contains(&GridChange::Ops { from }), "{changes:?}");
    assert!(changes.contains(&GridChange::Axis { sheet, axis: AxisKind::Cols }), "{changes:?}");
    assert_eq!(b.ops_from(from), vec![op], "the new entry reads back as the op written");
    assert!(!changes.iter().any(|c| matches!(c, GridChange::Axis { axis: AxisKind::Rows, .. })), "the other axis did not move");
}

#[test]
fn garbage_update_is_refused_and_the_doc_stays_sound() {
    let s = seed(3);
    let (mut a, mut b) = (peer(&s, 1), peer(&s, 2));
    let sheet = Id::base(0);
    let key = cell_key(Id::base(0), Id::base(0));
    let real = a.write(|t| t.set_cell(sheet, &key, &content(json!(42), "alice"))).unwrap();
    let before = b.state();
    assert!(b.apply(&[0xff, 0x01, 0x02]).is_err(), "bytes that are not an update are refused");
    assert_eq!(b.state(), before, "a refused update changes nothing");
    // Every cut of a real update: decodable prefixes and broken ones.
    for cut in 0..real.len() {
        let _ = b.apply(&real[..cut]);
        assert!(!b.damaged(), "a cut at {cut} leaves the replica sound");
    }
    b.apply(&real).unwrap();
    assert_eq!(b.cell(sheet, &key), a.cell(sheet, &key), "a real update still lands afterwards");
    assert!(b.write(|t| t.clear_cell(sheet, &key)).is_ok(), "and the replica still takes local edits");
}

/// Everything a replica shows, read through the public API over a small key space.
fn dump(d: &GridDoc) -> String {
    let mut out = format!("{:?}\n", d.meta());
    for si in 0..2 {
        let sheet = Id::base(si);
        out += &format!("{:?}\n", d.written_cells(sheet));
        for r in 0..4 {
            for c in 0..3 {
                let k = cell_key(Id::base(r), Id::base(c));
                out += &format!("{k} {:?} {:?}\n", d.cell(sheet, &k), d.fmt(sheet, &k));
            }
            let line = format!("{}/h", Id::base(r).text());
            out += &format!("{:?} {:?}\n", d.line(sheet, AxisKind::Rows, &line), d.line(sheet, AxisKind::Cols, &line));
        }
        out += &format!("{:?} {:?}\n", d.sheet_prop(sheet, "defw"), d.sheet_prop(sheet, "defh"));
    }
    for k in ["a", "b", "c"] {
        out += &format!("{:?}\n", d.style(k));
    }
    out
}

/// One random free write.
fn random_write(rng: &mut Rng, d: &mut GridDoc, who: &str) -> Vec<u8> {
    let sheet = Id::base(rng.below(2) as u32);
    let key = cell_key(Id::base(rng.below(4) as u32), Id::base(rng.below(3) as u32));
    let line = format!("{}/h", Id::base(rng.below(4) as u32).text());
    let n = rng.below(1000);
    let what = rng.below(11);
    d.write(|t| match what {
        0 | 1 => t.set_cell(sheet, &key, &content(json!(n), who)),
        2 => t.clear_cell(sheet, &key),
        3 => t.reset_cell(sheet, &key),
        4 => t.set_fmt(sheet, &key, ["a", "b", "c"][n % 3]),
        5 => t.reset_fmt(sheet, &key),
        6 => t.set_line(sheet, AxisKind::Rows, &line, json!(n)),
        7 => t.reset_line(sheet, AxisKind::Cols, &line),
        8 => t.set_sheet_prop(sheet, ["defw", "defh"][n % 2], json!(n)),
        9 => t.put_style(["a", "b", "c"][n % 3], json!({"b": n.is_multiple_of(2)})),
        _ => t.bump_epoch(who),
    })
    .unwrap()
}

#[test]
fn free_writes_converge_in_any_order() {
    for seed in seeds() {
        let mut rng = Rng(seed);
        let n = 2 + rng.below(4);
        let s = GridSeed { base_sha256: [seed as u8; 32], sheets: 2 };
        let mut docs: Vec<GridDoc> = (0..n).map(|i| peer(&s, i as u32 + 1)).collect();
        let mut log: Vec<Vec<u8>> = Vec::new();
        for _ in 0..60 {
            let who = rng.below(n);
            let u = random_write(&mut rng, &mut docs[who], &format!("peer{who}"));
            log.push(u);
            // Now and then a peer hears some of what was written so far, so later writes
            // build on earlier ones as well as racing them.
            if rng.chance(30) {
                let to = rng.below(n);
                for i in rng.order_with_duplicates(log.len()) {
                    if rng.chance(50) {
                        docs[to].apply(&log[i]).unwrap();
                    }
                }
            }
        }
        for d in &mut docs {
            for i in rng.order_with_duplicates(log.len()) {
                d.apply(&log[i]).unwrap();
            }
        }
        let first = dump(&docs[0]);
        for (i, d) in docs.iter().enumerate() {
            assert_eq!(dump(d), first, "seed {seed}: peer {i} reached the same document as peer 0");
        }
        let joiner = GridDoc::from_state(&docs[n - 1].state()).unwrap();
        assert_eq!(dump(&joiner), first, "seed {seed}: a joiner from a checkpoint reads the same");
    }
}
