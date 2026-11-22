//! Remote apply (H2), the live undo hook (H3) and quiet selection (H7): changes that arrive from
//! other peers reach the workbook without being authored here.

use std::sync::{Arc, Mutex};

use gridcraft_core::CellRef;
use gridcraft_engine::core::Value;
use gridcraft_engine::formula::adjust::Axis;
use gridcraft_engine::model::{Cell, LineInfo, Style};
use gridcraft_engine::{EditObserver, EditReport, EngineError, Selection, Session};
use serde_json::json;

fn at(a: &str) -> CellRef {
    CellRef::parse(a).unwrap()
}

fn open() -> (Session, u64) {
    let mut s = Session::new();
    s.execute("file.new", json!({})).unwrap();
    let uid = s.documents()[0].uid;
    (s, uid)
}

fn text(s: &Session, a: &str) -> String {
    s.documents()[0].wb.sheet(0).unwrap().input_text(at(a))
}

fn number(n: f64) -> Cell {
    Cell { value: Value::Number(n), formula: None, style: Default::default(), py: None }
}

#[derive(Clone, Default)]
struct Count(Arc<Mutex<usize>>);

impl EditObserver for Count {
    fn edited(&mut self, _: EditReport) {
        if let Ok(mut n) = self.0.lock() {
            *n += 1;
        }
    }
}

#[test]
fn apply_remote_adds_no_undo_step() {
    let (mut s, uid) = open();
    s.execute("cell.set", json!({"cell": "A1", "input": "1"})).unwrap();
    let (undo, redo) = (s.documents()[0].undo.len(), s.documents()[0].redo.len());
    s.apply_remote(uid, |cx| cx.set_cell(0, at("B2"), Some(number(5.0)))).unwrap();
    assert_eq!(s.documents()[0].undo.len(), undo);
    assert_eq!(s.documents()[0].redo.len(), redo);
    assert_eq!(text(&s, "B2"), "5");
    s.execute("edit.undo", json!({})).unwrap();
    assert_eq!(text(&s, "A1"), "");
}

#[test]
fn apply_remote_recalculates_dependents() {
    let (mut s, uid) = open();
    s.execute("cell.set", json!({"cell": "A1", "input": "1"})).unwrap();
    s.execute("cell.set", json!({"cell": "C1", "input": "=A1*2"})).unwrap();
    let value = |s: &Session| s.documents()[0].wb.sheet(0).unwrap().cell(at("C1")).map(|c| c.value.clone());
    let before = value(&s);
    s.apply_remote(uid, |cx| cx.set_cell(0, at("A1"), Some(number(21.0)))).unwrap();
    let after = value(&s);
    assert_eq!(after, Some(Value::Number(42.0)));
    assert_ne!(before, after);
}

#[test]
fn apply_remote_is_not_observed() {
    let (mut s, uid) = open();
    let tap = Count::default();
    s.set_observer(Some(Box::new(tap.clone())));
    s.apply_remote(uid, |cx| cx.set_cell(0, at("A1"), Some(number(1.0)))).unwrap();
    assert_eq!(*tap.0.lock().unwrap(), 0);
    s.execute("cell.set", json!({"cell": "A2", "input": "2"})).unwrap();
    assert_eq!(*tap.0.lock().unwrap(), 1, "local edits are still observed");
}

#[test]
fn apply_remote_keeps_the_selection() {
    let (mut s, uid) = open();
    s.execute("selection.set", json!({"range": "D4:E5", "active": "E5"})).unwrap();
    let sel = s.documents()[0].selection.clone();
    s.apply_remote(uid, |cx| cx.set_cell(0, at("D4"), Some(number(1.0)))).unwrap();
    assert_eq!(s.documents()[0].selection, sel);
}

#[test]
fn a_failed_remote_change_leaves_the_workbook_as_before() {
    let (mut s, uid) = open();
    let wb = s.documents()[0].wb.clone();
    let r: Result<(), EngineError> = s.apply_remote(uid, |cx| {
        cx.set_cell(0, at("A1"), Some(number(1.0)))?;
        cx.set_cell(99, at("A1"), Some(number(2.0)))
    });
    assert!(r.is_err());
    assert!(Arc::ptr_eq(&wb, &s.documents()[0].wb));
    let r = s.apply_remote(uid, |cx| cx.set_cell(0, CellRef::new(u32::MAX, 0), None));
    assert!(r.is_err(), "an address outside the grid is an error, not a panic");
    assert!(s.apply_remote(uid + 1000, |_| Ok(())).is_err());
}

#[test]
fn live_undo_hook_takes_undo_and_redo() {
    let (mut s, _) = open();
    s.execute("cell.set", json!({"cell": "A1", "input": "1"})).unwrap();
    let (undo, redo) = (s.documents()[0].undo.len(), s.documents()[0].redo.len());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let tap = seen.clone();
    s.set_live_undo_hook(Some(Box::new(move |redo| {
        if let Ok(mut v) = tap.lock() {
            v.push(redo);
        }
        true
    })));
    s.execute("edit.undo", json!({})).unwrap();
    s.execute("edit.redo", json!({})).unwrap();
    assert_eq!(*seen.lock().unwrap(), vec![false, true]);
    assert_eq!(s.documents()[0].undo.len(), undo);
    assert_eq!(s.documents()[0].redo.len(), redo);
    assert_eq!(text(&s, "A1"), "1");
    s.set_live_undo_hook(None);
    s.execute("edit.undo", json!({})).unwrap();
    assert_eq!(text(&s, "A1"), "", "without the hook the snapshot history works again");
}

#[test]
fn equal_formats_intern_to_one_style() {
    let (mut s, uid) = open();
    s.apply_remote(uid, |cx| {
        let mut bold = Style::default();
        bold.font.bold = true;
        let (a, b) = (cx.intern_style(&bold), cx.intern_style(&bold.clone()));
        assert_eq!(a, b);
        assert_eq!(cx.style_of(a), Some(&bold));
        assert_ne!(a, cx.intern_style(&Style::default()));
        Ok(())
    })
    .unwrap();
}

#[test]
fn remote_lines_and_sizes_apply_and_the_rest_is_quiet() {
    let (mut s, uid) = open();
    s.apply_remote(uid, |cx| {
        cx.set_line(0, Axis::Cols, 2, Some(LineInfo { size: Some(120.0), ..LineInfo::default() }))?;
        cx.set_default_size(0, Axis::Rows, Some(30.0))?;
        assert!(cx.set_default_size(0, Axis::Rows, Some(f32::NAN)).is_err());
        assert!(cx.insert_lines(0, Axis::Rows, 0, 1).is_err(), "not yet");
        Ok(())
    })
    .unwrap();
    let sh = s.documents()[0].wb.sheet(0).unwrap();
    assert_eq!(sh.cols.get(&2).and_then(|l| l.size), Some(120.0));
    assert_eq!(sh.default_row_height, 30.0);
    // The same workbook put back quietly.
    let wb = s.documents()[0].wb.clone();
    let tap = Count::default();
    s.set_observer(Some(Box::new(tap.clone())));
    s.replace_workbook_quiet(uid, wb.clone()).unwrap();
    assert_eq!(*tap.0.lock().unwrap(), 0);
    assert_eq!(s.documents()[0].wb.sheet(0).unwrap().cols.get(&2), wb.sheet(0).unwrap().cols.get(&2));
    assert!(s.replace_workbook_quiet(uid, Arc::new(Default::default())).is_err() || !s.documents()[0].wb.sheets.is_empty());
}

#[test]
fn quiet_selection_is_clamped_and_never_empty() {
    let (mut s, uid) = open();
    let far = CellRef::new(u32::MAX, u32::MAX);
    s.set_selection_quiet(uid, Selection { active: far, anchor: far, ranges: vec![] }).unwrap();
    let sel = &s.documents()[0].selection;
    assert!(sel.active.is_valid());
    assert!(!sel.ranges.is_empty() && sel.ranges.iter().all(|r| r.start.is_valid() && r.end.is_valid()));
    assert!(s.set_selection_quiet(uid + 1000, Selection::default()).is_err());
}
