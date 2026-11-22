//! Remote apply (H2), the live undo hook (H3) and quiet selection (H7): changes that arrive from
//! other peers reach the workbook without being authored here.

use std::sync::{Arc, Mutex};

use gridcraft_core::{CellRef, MAX_ROWS};
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
        assert!(cx.insert_lines(0, Axis::Rows, MAX_ROWS, 1).is_err(), "past the grid");
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
    let mut empty = (*wb).clone();
    empty.sheets.clear();
    assert!(s.replace_workbook_quiet(uid, Arc::new(empty)).is_err());
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

#[test]
fn remote_apply_during_an_open_group_is_refused_then_applies() {
    let (mut s, uid) = open();
    let token = s.group_begin("action").unwrap();
    let wb = s.documents()[0].wb.clone();
    let r = s.apply_remote(uid, |cx| cx.set_cell(0, at("B2"), Some(number(5.0))));
    assert!(matches!(r, Err(EngineError::RemoteBusy)), "{r:?}");
    assert!(Arc::ptr_eq(&wb, &s.documents()[0].wb));
    s.group_end(token).unwrap();
    s.apply_remote(uid, |cx| cx.set_cell(0, at("B2"), Some(number(5.0)))).unwrap();
    assert_eq!(text(&s, "B2"), "5");
}

#[test]
fn the_live_hook_serves_only_its_own_document() {
    let (mut s, _) = open();
    let calls = Arc::new(Mutex::new(0usize));
    let tap = calls.clone();
    s.set_live_undo_hook(Some(Box::new(move |_| {
        if let Ok(mut n) = tap.lock() {
            *n += 1;
        }
        true
    })));
    s.execute("file.new", json!({})).unwrap();
    s.execute("cell.set", json!({"cell": "A1", "input": "9"})).unwrap();
    s.execute("edit.undo", json!({})).unwrap();
    assert_eq!(*calls.lock().unwrap(), 0, "the second workbook uses snapshot undo");
    let second = s.active().unwrap().wb.sheet(0).unwrap().input_text(at("A1"));
    assert_eq!(second, "");
    s.set_active(0);
    s.execute("edit.undo", json!({})).unwrap();
    assert_eq!(*calls.lock().unwrap(), 1);
}

#[test]
fn a_panicking_hook_turns_undo_off_instead_of_falling_back() {
    let (mut s, uid) = open();
    s.execute("cell.set", json!({"cell": "A1", "input": "1"})).unwrap();
    s.set_live_undo_hook(Some(Box::new(|_| panic!("hook failed"))));
    assert!(!s.live_undo_failed());
    s.execute("edit.undo", json!({})).unwrap();
    assert!(s.live_undo_failed());
    s.execute("edit.undo", json!({})).unwrap();
    assert_eq!(text(&s, "A1"), "1", "no snapshot undo while live");
    assert_eq!(s.documents()[0].undo.len(), 1);
    s.apply_remote(uid, |_| Ok(())).unwrap();
    s.set_live_undo_hook(None);
    assert!(!s.live_undo_failed());
}

#[test]
fn a_hook_answering_false_reports_nothing_undone() {
    let (mut s, _) = open();
    s.set_live_undo_hook(Some(Box::new(|_| false)));
    let r = s.execute("edit.undo", json!({})).unwrap();
    assert!(r["undone"].is_null());
    let r = s.execute("edit.redo", json!({})).unwrap();
    assert!(r["redone"].is_null());
}

#[test]
fn a_remote_formula_follows_its_local_precedent() {
    let (mut s, uid) = open();
    let formula = {
        let mut other = Session::new();
        other.execute("file.new", json!({})).unwrap();
        other.execute("cell.set", json!({"cell": "C1", "input": "=A1*2"})).unwrap();
        other.documents()[0].wb.sheet(0).unwrap().cell(at("C1")).cloned().unwrap()
    };
    s.apply_remote(uid, |cx| cx.set_cell(0, at("C1"), Some(formula))).unwrap();
    s.execute("cell.set", json!({"cell": "A1", "input": "4"})).unwrap();
    // The same edits made locally give the same result.
    let (mut l, _) = open();
    l.execute("cell.set", json!({"cell": "C1", "input": "=A1*2"})).unwrap();
    l.execute("cell.set", json!({"cell": "A1", "input": "4"})).unwrap();
    let v = |s: &Session| s.documents()[0].wb.sheet(0).unwrap().cell(at("C1")).map(|c| c.value.clone());
    assert_eq!(v(&s), v(&l));
}

#[test]
fn bad_remote_styles_and_sizes_are_refused() {
    let (mut s, uid) = open();
    let wb = s.documents()[0].wb.clone();
    let bad_style = Cell { style: gridcraft_engine::model::StyleId(u32::MAX), ..number(1.0) };
    assert!(s.apply_remote(uid, |cx| cx.set_cell(0, at("A1"), Some(bad_style))).is_err());
    let info = |size, style| LineInfo { size, style, ..LineInfo::default() };
    let missing = Some(gridcraft_engine::model::StyleId(u32::MAX));
    assert!(s.apply_remote(uid, |cx| cx.set_line(0, Axis::Rows, 0, Some(info(None, missing)))).is_err());
    assert!(s.apply_remote(uid, |cx| cx.set_line(0, Axis::Rows, 0, Some(info(Some(f32::NAN), None)))).is_err());
    assert!(s.apply_remote(uid, |cx| cx.set_line(0, Axis::Rows, 0, Some(info(Some(-1.0), None)))).is_err());
    assert!(s.apply_remote(uid, |cx| cx.set_default_size(0, Axis::Cols, Some(0.0))).is_err());
    assert!(Arc::ptr_eq(&wb, &s.documents()[0].wb));
}

#[test]
fn a_quiet_replace_during_an_open_group_is_refused_then_applies() {
    let (mut s, uid) = open();
    let token = s.group_begin("action").unwrap();
    let wb = s.documents()[0].wb.clone();
    let r = s.replace_workbook_quiet(uid, wb.clone());
    assert!(matches!(r, Err(EngineError::RemoteBusy)), "{r:?}");
    assert!(Arc::ptr_eq(&wb, &s.documents()[0].wb));
    s.group_end(token).unwrap();
    s.replace_workbook_quiet(uid, wb).unwrap();
}

#[test]
fn a_hook_can_be_bound_to_a_named_document_and_ends_with_it() {
    let (mut s, first) = open();
    s.execute("file.new", json!({})).unwrap();
    let second = s.active().unwrap().uid;
    assert_ne!(first, second);
    assert!(s.set_live_undo_hook_for(second + 1000, Some(Box::new(|_| true))).is_err());
    assert_eq!(s.live_undo_doc(), None, "a refused call binds nothing");
    // Bound to the first document while the second is active: the active one keeps snapshot undo.
    s.set_live_undo_hook_for(first, Some(Box::new(|_| true))).unwrap();
    assert_eq!(s.live_undo_doc(), Some(first));
    s.execute("cell.set", json!({"cell": "A1", "input": "3"})).unwrap();
    s.execute("edit.undo", json!({})).unwrap();
    assert_eq!(s.active().unwrap().wb.sheet(0).unwrap().input_text(at("A1")), "");
    // Closing another document keeps the binding; closing the bound one drops it.
    s.close_document(1);
    assert_eq!(s.live_undo_doc(), Some(first));
    s.close_document(0);
    assert_eq!(s.live_undo_doc(), None);
    assert!(!s.live_undo_failed());
}
