//! H6: a volatile source makes `RAND` a function of the cell and the draw, not of the history.

use std::sync::Arc;

use gridcraft_core::{CellRef, Value};
use gridcraft_engine::Session;
use gridcraft_engine::calc::VolatileSource;
use serde_json::json;

/// A hash of its arguments; the clock stays local.
#[derive(Debug)]
struct Hashed(u64);

impl VolatileSource for Hashed {
    fn random(&self, sheet: usize, cell: CellRef, draw: u32) -> f64 {
        let mut x = self.0;
        for part in [sheet as u64, u64::from(cell.row), u64::from(cell.col), u64::from(draw)] {
            x = x.wrapping_add(part).wrapping_add(0x9E37_79B9_7F4A_7C15);
            x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            x ^= x >> 31;
        }
        (x >> 11) as f64 / (1u64 << 53) as f64
    }
}

fn session(source: Option<Arc<dyn VolatileSource>>) -> Session {
    let mut s = Session::new();
    s.new_workbook();
    if source.is_some() {
        let uid = s.doc().unwrap().uid;
        s.set_volatile_source(uid, source).unwrap();
    }
    s
}

fn set(s: &mut Session, cell: &str, input: &str) {
    s.execute("cell.set", json!({"cell": cell, "input": input})).unwrap();
}

fn num(s: &Session, row: u32, col: u32) -> f64 {
    match s.doc().unwrap().wb.sheet(0).unwrap().value(CellRef::new(row, col)) {
        Value::Number(n) => n,
        other => panic!("not a number at {row},{col}: {other:?}"),
    }
}

const CELLS: [(&str, &str); 4] = [("A1", "=RAND()"), ("A2", "=RAND()"), ("B1", "=RANDBETWEEN(1,1000000)"), ("C3", "=RAND()+RAND()")];

#[test]
fn without_a_source_rand_changes_between_recalcs() {
    let mut s = session(None);
    set(&mut s, "A1", "=RAND()");
    let before = num(&s, 0, 0);
    set(&mut s, "B5", "1");
    assert_ne!(before, num(&s, 0, 0));
}

#[test]
fn same_source_same_numbers_whatever_the_history() {
    let source: Arc<dyn VolatileSource> = Arc::new(Hashed(7));
    let mut a = session(Some(source.clone()));
    let mut b = session(Some(source));
    for (cell, f) in CELLS {
        set(&mut a, cell, f);
    }
    for (cell, f) in CELLS.iter().rev() {
        set(&mut b, cell, f);
    }
    for i in 0..3 {
        set(&mut b, "E9", &i.to_string());
    }
    for (r, c) in [(0, 0), (1, 0), (0, 1), (2, 2)] {
        assert_eq!(num(&a, r, c).to_bits(), num(&b, r, c).to_bits(), "cell {r},{c}");
    }
}

#[test]
fn two_rands_in_one_formula_differ() {
    let mut s = session(Some(Arc::new(Hashed(1))));
    set(&mut s, "A1", "=RAND()-RAND()");
    assert_ne!(num(&s, 0, 0), 0.0);
}

#[test]
fn randarray_elements_differ() {
    let mut s = session(Some(Arc::new(Hashed(3))));
    set(&mut s, "A1", "=RANDARRAY(1,6)");
    let v: Vec<u64> = (0..6).map(|c| num(&s, 0, c).to_bits()).collect();
    for i in 0..v.len() {
        for j in i + 1..v.len() {
            assert_ne!(v[i], v[j], "elements {i} and {j}");
        }
    }
}

#[test]
fn randbetween_stays_in_bounds_with_a_source() {
    let mut s = session(Some(Arc::new(Hashed(11))));
    for r in 1..=40 {
        set(&mut s, &format!("A{r}"), "=RANDBETWEEN(3,9)");
    }
    for r in 0..40 {
        let n = num(&s, r, 0);
        assert!((3.0..=9.0).contains(&n) && n.fract() == 0.0, "row {r}: {n}");
    }
}
