//! Remote line inserts and deletes (H5 for rows and columns): applied by position, with no undo
//! step, to the same result as the local command; reports replay them instead of naming a
//! structure change.

use std::sync::{Arc, Mutex};

use gridcraft_core::{CellRef, MAX_COLS, MAX_ROWS};
use gridcraft_engine::formula::adjust::Axis;
use gridcraft_engine::model::Workbook;
use gridcraft_engine::{EditObserver, EditReport, Session};
use serde_json::json;

fn at(a: &str) -> CellRef {
    CellRef::parse(a).unwrap()
}

fn run(s: &mut Session, id: &str, p: serde_json::Value) {
    s.execute(id, p).unwrap_or_else(|e| panic!("{id}: {e}"));
}

/// A workbook with formulas across two sheets, merges, conditional formats, a chart anchored
/// below the edited lines and custom line sizes.
fn build() -> Session {
    let mut s = Session::new();
    run(&mut s, "file.new", json!({}));
    for r in 1..=12 {
        run(&mut s, "cell.set", json!({"cell": format!("A{r}"), "input": r.to_string()}));
        run(&mut s, "cell.set", json!({"cell": format!("B{r}"), "input": format!("=A{r}*2")}));
    }
    run(&mut s, "cell.set", json!({"cell": "E1", "input": "=SUM(A1:A12)"}));
    run(&mut s, "cell.set", json!({"cell": "F9", "input": "=B10+Sheet2!B1"}));
    run(&mut s, "home.mergeCells", json!({"range": "C5:D6"}));
    run(&mut s, "home.mergeCells", json!({"range": "H2:H3"}));
    let rule = json!({"type": "cellIs", "operator": "greaterThan", "value": "5", "preset": "lightRedFill"});
    run(&mut s, "home.conditionalFormat", json!({"range": "A4:B9", "rule": rule}));
    run(&mut s, "home.rowHeight", json!({"rows": "2:2", "height": 40}));
    run(&mut s, "home.rowHeight", json!({"rows": "8:8", "height": 55}));
    run(&mut s, "home.columnWidth", json!({"cols": "B:B", "width": 120}));
    run(&mut s, "home.columnWidth", json!({"cols": "E:E", "width": 90}));
    run(&mut s, "insert.chart", json!({"type": "bar", "at": "M30", "width": 200, "height": 100}));
    run(&mut s, "range.setValues", json!({"range": "J2", "values": [["Kind", "Cost", "Note"], ["a", 1, "x"], ["b", 2, "y"]]}));
    run(&mut s, "insert.table", json!({"range": "J2:L4", "header": true}));
    run(&mut s, "cell.set", json!({"cell": "N2", "input": "=SUM(Table1[Cost])"}));
    run(&mut s, "home.insertSheet", json!({"name": "Sheet2"}));
    run(&mut s, "cell.set", json!({"cell": "A1", "input": "=Sheet1!A6+Sheet1!B11"}));
    run(&mut s, "cell.set", json!({"cell": "B1", "input": "=SUM(Sheet1!A1:A12)"}));
    run(&mut s, "sheet.activate", json!({"sheet": 0}));
    s
}

/// The same workbook in a second session, as a peer would hold it.
fn peer_of(s: &Session) -> (Session, u64) {
    let mut p = Session::new();
    run(&mut p, "file.new", json!({}));
    let uid = p.documents()[0].uid;
    p.replace_workbook_quiet(uid, s.documents()[0].wb.clone()).unwrap();
    (p, uid)
}

/// The whole workbook as JSON, less each person's own view (zoom, selection, scroll and so on).
fn describe(wb: &Workbook) -> serde_json::Value {
    const LOCAL: &[&str] = &[
        "zoom",
        "show_formulas",
        "show_gridlines",
        "show_headings",
        "show_zeros",
        "show_outline_symbols",
        "view_active",
        "view_top_left",
        "view",
        "active_sheet",
        "show_objects",
    ];
    let mut v = serde_json::to_value(wb).unwrap();
    if let Some(m) = v.as_object_mut() {
        m.retain(|k, _| !LOCAL.contains(&k.as_str()));
        if let Some(sheets) = m.get_mut("sheets").and_then(|s| s.as_array_mut()) {
            for sh in sheets.iter_mut().filter_map(|s| s.as_object_mut()) {
                sh.retain(|k, _| !LOCAL.contains(&k.as_str()));
            }
        }
    }
    v
}

fn assert_same(a: &Workbook, b: &Workbook) {
    // Cached results are each peer's own recalculation; everything else must be equal.
    let (x, y) = (describe(a), describe(b));
    if x != y {
        let sheets = |v: &serde_json::Value| v["sheets"].as_array().cloned().unwrap_or_default();
        for (i, (p, q)) in sheets(&x).iter().zip(sheets(&y)).enumerate() {
            for (k, pv) in p.as_object().into_iter().flatten() {
                assert_eq!(pv, &q[k], "sheet {i} field `{k}` differs");
            }
        }
        assert_eq!(x, y, "workbook-level fields differ");
    }
}

#[test]
fn remote_insert_equals_local_insert() {
    let mut a = build();
    let (mut b, uid) = peer_of(&a);
    let before = describe(&a.documents()[0].wb);
    assert!(!a.documents()[0].wb.sheet(0).unwrap().tables.is_empty(), "the workbook has a table");
    run(&mut a, "home.insertRows", json!({"rows": "3:4"}));
    b.apply_remote(uid, |cx| cx.insert_lines(0, Axis::Rows, 2, 2)).unwrap();
    assert_ne!(before, describe(&a.documents()[0].wb), "the insert must change something");
    assert_same(&a.documents()[0].wb, &b.documents()[0].wb);
}

#[test]
fn remote_insert_of_columns_equals_local_insert() {
    let mut a = build();
    let (mut b, uid) = peer_of(&a);
    run(&mut a, "home.insertColumns", json!({"cols": "B:C"}));
    b.apply_remote(uid, |cx| cx.insert_lines(0, Axis::Cols, 1, 2)).unwrap();
    assert_same(&a.documents()[0].wb, &b.documents()[0].wb);
}

#[test]
fn remote_delete_equals_local_delete() {
    let mut a = build();
    let (mut b, uid) = peer_of(&a);
    run(&mut a, "home.deleteRows", json!({"rows": "2:3,7:8"}));
    b.apply_remote(uid, |cx| cx.delete_lines(0, Axis::Rows, &[(6, 7), (1, 2)])).unwrap();
    assert_same(&a.documents()[0].wb, &b.documents()[0].wb);

    // Areas arrive in any order, reversed (first > last), touching and overlapping.
    let mut a = build();
    let (mut b, uid) = peer_of(&a);
    run(&mut a, "home.deleteRows", json!({"rows": "2:4,6:6"}));
    b.apply_remote(uid, |cx| cx.delete_lines(0, Axis::Rows, &[(3, 1), (2, 2), (5, 5)])).unwrap();
    assert_same(&a.documents()[0].wb, &b.documents()[0].wb);

    let mut a = build();
    let (mut b, uid) = peer_of(&a);
    run(&mut a, "home.deleteColumns", json!({"cols": "B:B,D:E"}));
    b.apply_remote(uid, |cx| cx.delete_lines(0, Axis::Cols, &[(1, 1), (3, 4)])).unwrap();
    assert_same(&a.documents()[0].wb, &b.documents()[0].wb);
}

#[test]
fn remote_lines_add_no_undo_step() {
    let mut s = build();
    let uid = s.documents()[0].uid;
    let (undo, redo) = (s.documents()[0].undo.len(), s.documents()[0].redo.len());
    s.apply_remote(uid, |cx| cx.insert_lines(0, Axis::Rows, 2, 2)).unwrap();
    s.apply_remote(uid, |cx| cx.delete_lines(0, Axis::Cols, &[(1, 1)])).unwrap();
    assert_eq!(s.documents()[0].undo.len(), undo);
    assert_eq!(s.documents()[0].redo.len(), redo);
    // The inserted rows are a gap and the old row 3 sits below it.
    let sh = s.documents()[0].wb.sheet(0).unwrap();
    assert_eq!(sh.input_text(at("A3")), "");
    assert_eq!(sh.input_text(at("A5")), "3");
}

#[derive(Clone, Default)]
struct Tap(Arc<Mutex<Vec<EditReport>>>);

impl EditObserver for Tap {
    fn edited(&mut self, report: EditReport) {
        if let Ok(mut all) = self.0.lock() {
            all.push(report);
        }
    }
}

fn one(s: &mut Session, tap: &Tap, id: &str, p: serde_json::Value) -> EditReport {
    tap.0.lock().unwrap().clear();
    run(s, id, p);
    let mut all = std::mem::take(&mut *tap.0.lock().unwrap());
    assert_eq!(all.len(), 1);
    all.remove(0)
}

#[test]
fn structural_report_replays_to_after() {
    let mut a = build();
    let (mut b, uid) = peer_of(&a);
    let tap = Tap::default();
    a.set_observer(Some(Box::new(tap.clone())));
    // Row 2 is 40 high: the inserted rows take that format.
    let r = one(&mut a, &tap, "home.insertRows", json!({"rows": "3:4"}));
    assert!(r.unshared.is_empty(), "{:?}", r.unshared);
    assert!(r.cells.is_empty(), "an insert is no cell write: {:?}", r.cells);
    assert!(r.lines.is_empty(), "the inherited format is part of the replay: {:?}", r.lines);
    b.apply_remote(uid, |cx| {
        for (sheet, e) in &r.edits {
            let gridcraft_engine::formula::adjust::Edit::Insert { axis, at, count } = e else { panic!("not an insert") };
            cx.insert_lines(*sheet, *axis, *at, *count)?;
        }
        for &(sheet, c) in &r.cells {
            cx.set_cell(sheet, c, r.after.sheet(sheet).and_then(|sh| sh.cell(c)).cloned())?;
        }
        for &(sheet, axis, i) in &r.lines {
            let sh = r.after.sheet(sheet).unwrap();
            let info = if axis == Axis::Rows { sh.rows.get(&i) } else { sh.cols.get(&i) };
            cx.set_line(sheet, axis, i, info.copied())?;
        }
        Ok(())
    })
    .unwrap();
    assert_same(&r.after, &b.documents()[0].wb);

    // A multi-area delete: its report is empty too, and applying its edits (one call per edit)
    // gives the author's workbook.
    let r = one(&mut a, &tap, "home.deleteRows", json!({"rows": "2:3,7:8"}));
    assert!(r.unshared.is_empty(), "{:?}", r.unshared);
    assert!(r.cells.is_empty() && r.lines.is_empty(), "{:?} {:?}", r.cells, r.lines);
    assert_eq!(r.edits.len(), 2, "one edit per area");
    let (mut c, cuid) = (Session::new(), 0);
    run(&mut c, "file.new", json!({}));
    let cuid = c.documents()[0].uid + cuid;
    c.replace_workbook_quiet(cuid, r.before.clone()).unwrap();
    c.apply_remote(cuid, |cx| {
        for (sheet, e) in &r.edits {
            let gridcraft_engine::formula::adjust::Edit::Delete { axis, at, count } = e else { panic!("not a delete") };
            cx.delete_lines(*sheet, *axis, &[(*at, *at + *count - 1)])?;
        }
        Ok(())
    })
    .unwrap();
    assert_same(&r.after, &c.documents()[0].wb);
}

#[test]
fn lines_inside_a_table_keep_its_header_in_step() {
    // A column inserted inside the table is named and written into the header as the local
    // command does, and a deleted header row promotes the next row.
    for (id, params, axis, at, count) in [
        ("home.insertColumns", json!({"cols": "K:K"}), Axis::Cols, 10, 1),
        ("home.deleteRows", json!({"rows": "2:2"}), Axis::Rows, 1, 1),
        ("home.insertRows", json!({"rows": "3:3"}), Axis::Rows, 2, 1),
    ] {
        let mut a = build();
        let (mut b, uid) = peer_of(&a);
        run(&mut a, id, params);
        b.apply_remote(uid, |cx| {
            if id == "home.deleteRows" { cx.delete_lines(0, axis, &[(at, at + count - 1)]) } else { cx.insert_lines(0, axis, at, count) }
        })
        .unwrap();
        assert_same(&a.documents()[0].wb, &b.documents()[0].wb);
    }
    // No report names Tables for a plain insert.
    let mut a = build();
    let tap = Tap::default();
    a.set_observer(Some(Box::new(tap.clone())));
    let r = one(&mut a, &tap, "home.insertColumns", json!({"cols": "K:K"}));
    assert!(!r.unshared.contains(&gridcraft_engine::Unshared::Tables(0)), "{:?}", r.unshared);
}

#[test]
fn replay_of_other_sheets_is_not_a_change() {
    // Rows inserted on sheet 1: formulas, a defined name and a chart on sheet 2 follow, which is
    // no write of sheet 2, no name change and no object change.
    let mut s = build();
    run(&mut s, "formulas.defineName", json!({"name": "Data", "refersTo": "=Sheet1!$A$1:$A$12"}));
    // A chart on sheet 2 whose series read sheet 1.
    run(&mut s, "insert.chart", json!({"type": "bar", "at": "P40", "width": 200, "height": 100}));
    let uid = s.documents()[0].uid;
    let mut wb = Workbook::clone(&s.documents()[0].wb);
    let mut chart = wb.sheet_mut(0).unwrap().charts.pop().unwrap();
    assert!(!chart.series.is_empty());
    chart.series[0].values = "Sheet1!$A$4:$A$9".into();
    wb.sheet_mut(1).unwrap().charts.push(chart);
    s.replace_workbook_quiet(uid, Arc::new(wb)).unwrap();
    let tap = Tap::default();
    s.set_observer(Some(Box::new(tap.clone())));
    let wb0 = s.documents()[0].wb.clone();
    let r = one(&mut s, &tap, "home.insertRows", json!({"rows": "3:4"}));
    assert_eq!(wb0.sheet(1).unwrap().charts.len(), 1, "the chart is on sheet 2");
    assert_ne!(wb0.sheet(1).unwrap().charts, r.after.sheet(1).unwrap().charts, "the chart's reference moved");
    assert_ne!(wb0.names, r.after.names, "the name's reference moved");
    assert_ne!(wb0.sheet(1).unwrap().input_text(at("A1")), r.after.sheet(1).unwrap().input_text(at("A1")), "so did the formula");
    assert!(!r.unshared.contains(&gridcraft_engine::Unshared::Names), "{:?}", r.unshared);
    assert!(!r.unshared.contains(&gridcraft_engine::Unshared::Objects(1)), "{:?}", r.unshared);
    assert!(r.cells.iter().all(|(sheet, _)| *sheet != 1), "{:?}", r.cells);
    assert!(r.unshared.is_empty(), "{:?}", r.unshared);
}

#[test]
fn insert_past_the_sheet_end_is_refused_remotely_too() {
    let mut s = build();
    let uid = s.documents()[0].uid;
    run(&mut s, "cell.set", json!({"cell": format!("A{MAX_ROWS}"), "input": "last"}));
    let before = describe(&s.documents()[0].wb);
    // Cells would be pushed off the grid.
    assert!(s.apply_remote(uid, |cx| cx.insert_lines(0, Axis::Rows, 0, 1)).is_err());
    // Past the grid, or junk.
    assert!(s.apply_remote(uid, |cx| cx.insert_lines(0, Axis::Rows, MAX_ROWS - 1, 5)).is_err());
    assert!(s.apply_remote(uid, |cx| cx.insert_lines(0, Axis::Cols, MAX_COLS, 1)).is_err());
    assert!(s.apply_remote(uid, |cx| cx.insert_lines(0, Axis::Rows, u32::MAX, u32::MAX)).is_err());
    assert!(s.apply_remote(uid, |cx| cx.insert_lines(0, Axis::Rows, 3, 0)).is_err());
    assert!(s.apply_remote(uid, |cx| cx.insert_lines(9, Axis::Rows, 3, 1)).is_err());
    assert!(s.apply_remote(uid, |cx| cx.delete_lines(0, Axis::Rows, &[(0, u32::MAX)])).is_err());
    assert!(s.apply_remote(uid, |cx| cx.delete_lines(0, Axis::Cols, &[(MAX_COLS, MAX_COLS)])).is_err());
    assert!(s.apply_remote(uid, |cx| cx.delete_lines(9, Axis::Rows, &[(1, 1)])).is_err());
    assert_eq!(before, describe(&s.documents()[0].wb), "a refused change leaves the workbook alone");
    // A valid one still works afterwards.
    s.apply_remote(uid, |cx| cx.delete_lines(0, Axis::Rows, &[(0, 0)])).unwrap();
    assert_eq!(s.documents()[0].wb.sheet(0).unwrap().input_text(at("A1")), "2");
}
