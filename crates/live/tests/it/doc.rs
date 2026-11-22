//! The grid document: the seed, absent against cleared, what a remote update names, hostile
//! bytes, and replicas that reach the same document whatever order their updates arrive in.

use craft_collab::Replica;
use gridcraft_live::{AxisKind, CellContent, Entry, GridChange, GridDoc, GridSeed, HELD_MAX_BYTES, HELD_MAX_ENTRIES, Id, Op, cell_key};
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
    // Seeded random bytes, some starting like the real update.
    for seed in seeds() {
        let mut rng = Rng(seed);
        for _ in 0..20 {
            let mut junk: Vec<u8> = (0..rng.below(64)).map(|_| rng.next_u64() as u8).collect();
            if rng.chance(30) {
                let keep = rng.below(real.len());
                junk.splice(0..0, real[..keep].iter().copied());
            }
            let _ = b.apply(&junk);
            assert!(!b.damaged(), "seed {seed}: random bytes leave the replica sound");
        }
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

#[test]
fn a_gap_that_never_closes_is_bounded_and_asks_for_a_full_state() {
    let s = seed(4);
    let (mut a, mut b) = (peer(&s, 1), peer(&s, 2));
    let sheet = Id::base(0);
    // The value every later write to the same key names as its origin; `b` never hears it.
    // (yrs integrates across a clock gap when an item's origins are known, so the dependency
    // has to be an origin.)
    let key = cell_key(Id::base(0), Id::base(0));
    let _withheld = a.write(|t| t.set_cell(sheet, &key, &content(json!(-1), "alice"))).unwrap();
    let mut most_held = 0;
    let mut most_bytes = 0;
    for n in 0..(HELD_MAX_ENTRIES * 2) {
        let u = a.write(|t| t.set_cell(sheet, &key, &content(json!(n), "alice"))).unwrap();
        b.apply(&u).unwrap();
        most_held = most_held.max(b.held_len());
        most_bytes = most_bytes.max(b.held_bytes());
    }
    assert!(most_held <= HELD_MAX_ENTRIES, "the copies never outgrow their cap");
    assert!(most_bytes <= HELD_MAX_BYTES, "nor their byte cap");
    assert!(b.needs_resync(), "a gap that outgrew the cap asks for a full state");
    assert_ne!(dump(&b), dump(&a), "the stuck replica is behind");
    b.apply(&a.state()).unwrap();
    b.clear_resync();
    assert_eq!(dump(&b), dump(&a), "after a full state the replicas agree");
    assert_eq!(b.held_len(), 0, "and nothing is held any more");
    assert!(!b.needs_resync());
}

#[test]
fn writes_to_a_sheet_the_doc_lacks_are_counted() {
    let s = seed(5);
    let mut d = peer(&s, 1);
    let gone = Id::base(s.sheets + 3);
    let key = cell_key(Id::base(0), Id::base(0));
    d.write(|t| {
        t.set_cell(gone, &key, &content(json!(1), "alice"));
        t.set_line(gone, AxisKind::Rows, "1/h", json!(20));
    })
    .unwrap();
    assert!(d.write_skipped() >= 2, "both writes to the missing sheet are counted");
    assert_eq!(d.cell(gone, &key), Entry::Absent, "and nothing was written");
    d.write(|t| t.set_cell(Id::base(0), &key, &content(json!(1), "alice"))).unwrap();
    assert_eq!(d.write_skipped(), 0, "a write that lands counts nothing, and the count is per write");
}

#[test]
fn styles_props_and_meta_are_named_and_converge() {
    let s = seed(6);
    let (mut a, mut b) = (peer(&s, 1), peer(&s, 2));
    let sheet = Id::base(1);
    // One write each, seen by the other: each change is named.
    let u = a.write(|t| t.put_style("k1", json!({"b": true}))).unwrap();
    assert!(b.apply(&u).unwrap().contains(&GridChange::Styles { keys: vec!["k1".into()] }));
    let u = a.write(|t| t.set_sheet_prop(sheet, "defw", json!(12))).unwrap();
    assert!(b.apply(&u).unwrap().contains(&GridChange::SheetProps { sheet }));
    let u = a.write(|t| t.bump_epoch("alice")).unwrap();
    assert!(b.apply(&u).unwrap().contains(&GridChange::Meta));
    // Then both write the same style, prop and epoch at once, and hear each other in either order.
    let ua = a
        .write(|t| {
            t.put_style("k1", json!({"i": true}));
            t.set_sheet_prop(sheet, "defw", json!(20));
            t.bump_epoch("alice");
        })
        .unwrap();
    let ub = b
        .write(|t| {
            t.put_style("k1", json!({"u": true}));
            t.set_sheet_prop(sheet, "defw", json!(30));
            t.bump_epoch("bob");
        })
        .unwrap();
    let mut c = GridDoc::from_state(&a.state()).unwrap();
    b.apply(&ua).unwrap();
    a.apply(&ub).unwrap();
    c.apply(&ub).unwrap();
    c.apply(&ua).unwrap();
    for d in [&b, &c] {
        assert_eq!(d.style("k1"), a.style("k1"), "one style wins everywhere");
        assert_eq!(d.sheet_prop(sheet, "defw"), a.sheet_prop(sheet, "defw"), "one prop wins everywhere");
        assert_eq!(d.meta(), a.meta(), "one epoch wins everywhere");
    }
    assert_eq!(dump(&b), dump(&a));
}
