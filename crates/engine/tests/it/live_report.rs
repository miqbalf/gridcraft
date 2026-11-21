//! The edit observer (H1) and its unshared-change safety net (H8): one report per top-level
//! command that changed a workbook, naming what changed and what M1 does not share.

use std::sync::{Arc, Mutex};

use gridcraft_core::CellRef;
use gridcraft_engine::formula::adjust::{Axis, Edit};
use gridcraft_engine::{EditObserver, EditReport, Session, Unshared, command_specs};
use serde_json::{Value as Json, json};

/// Collects what the observer hears.
#[derive(Clone, Default)]
struct Tap(Arc<Mutex<Vec<EditReport>>>);

impl EditObserver for Tap {
    fn edited(&mut self, report: EditReport) {
        if let Ok(mut all) = self.0.lock() {
            all.push(report);
        }
    }
}

impl Tap {
    /// Everything heard since the last call.
    fn take(&self) -> Vec<EditReport> {
        self.0.lock().map(|mut all| std::mem::take(&mut *all)).unwrap_or_default()
    }
}

fn sample(name: &str) -> (Session, Tap) {
    let mut s = Session::new();
    s.execute("file.new", json!({"sample": name})).unwrap();
    let tap = Tap::default();
    s.set_observer(Some(Box::new(tap.clone())));
    (s, tap)
}

fn at(a: &str) -> CellRef {
    CellRef::parse(a).unwrap()
}

/// Runs one command and returns its single report.
fn one(s: &mut Session, tap: &Tap, id: &str, params: Json) -> EditReport {
    tap.take();
    s.execute(id, params).unwrap();
    let mut all = tap.take();
    assert_eq!(all.len(), 1, "{id} must report exactly once");
    all.remove(0)
}

#[test]
fn every_mutating_command_reports_once() {
    let (mut s, tap) = sample("budget");
    // The command, its parameters, and what its report must include.
    enum Target {
        Cell(&'static str),
        Line(Axis, u32),
    }
    let cases = [
        ("cell.set", json!({"cell": "C5", "input": "7"}), Target::Cell("C5")),
        ("range.setValues", json!({"range": "B2", "values": [["changed"]]}), Target::Cell("B2")),
        ("edit.clearContents", json!({"range": "C6"}), Target::Cell("C6")),
        ("home.bold", json!({"range": "C7"}), Target::Cell("C7")),
        ("home.numberFormat", json!({"range": "C8", "format": "0.0000"}), Target::Cell("C8")),
        ("home.rowHeight", json!({"rows": "9:9", "height": 77}), Target::Line(Axis::Rows, 8)),
        ("home.columnWidth", json!({"cols": "H:H", "width": 150}), Target::Line(Axis::Cols, 7)),
        ("home.hideRows", json!({"rows": "10:10"}), Target::Line(Axis::Rows, 9)),
        ("edit.paste", json!({"at": "C11", "text": "1\t2"}), Target::Cell("C11")),
    ];
    for (id, params, target) in cases {
        let r = one(&mut s, &tap, id, params);
        assert_eq!(r.command, id);
        assert!(!Arc::ptr_eq(&r.before, &r.after), "{id}: before and after must differ");
        match target {
            Target::Cell(a) => assert!(r.cells.contains(&(0, at(a))), "{id}: cells {:?} must include {a}", r.cells),
            Target::Line(axis, i) => assert!(r.lines.contains(&(0, axis, i)), "{id}: lines {:?} must include {i}", r.lines),
        }
    }
}

#[test]
fn a_query_or_no_op_reports_nothing() {
    let (mut s, tap) = sample("budget");
    s.execute("formulas.functions", json!({})).unwrap();
    s.execute("edit.copy", json!({"range": "C5"})).unwrap();
    // Typing what is already there changes nothing.
    s.execute("cell.set", json!({"cell": "C5", "input": "1850"})).unwrap();
    assert!(tap.take().is_empty());
}

#[test]
fn nested_commands_report_as_one() {
    let (mut s, tap) = sample("budget");
    s.execute("automate.newScript", json!({"name": "grow", "commands": [{"command": "home.insertRows", "params": {"rows": "3:3"}}]})).unwrap();
    tap.take();
    s.execute("automate.runScript", json!({"name": "grow"})).unwrap();
    let all = tap.take();
    assert_eq!(all.len(), 1, "the script and the insert it ran are one command");
    assert!(
        matches!(all[0].edits.as_slice(), [(0, Edit::Insert { axis: Axis::Rows, count: 1, .. })]),
        "the inner insert is in the report: {:?}",
        all[0].edits
    );
}

#[test]
fn format_only_change_is_in_cells() {
    let (mut s, tap) = sample("budget");
    let r = one(&mut s, &tap, "home.bold", json!({"range": "C6", "on": true}));
    assert_eq!(r.cells, vec![(0, at("C6"))]);
    assert!(r.lines.is_empty() && r.edits.is_empty() && r.unshared.is_empty());
    let (b, a) = (r.before.sheets[0].cell(at("C6")), r.after.sheets[0].cell(at("C6")));
    assert_eq!(b.map(|c| &c.value), a.map(|c| &c.value), "the value is the same");
    assert_ne!(b.map(|c| c.style), a.map(|c| c.style), "the style is not");
}

#[test]
fn unshared_changes_are_named() {
    let (mut s, tap) = sample("budget");
    let r = one(&mut s, &tap, "home.mergeCells", json!({"range": "B20:C20"}));
    assert!(r.unshared.contains(&Unshared::Merges(0)));
    let r = one(&mut s, &tap, "insert.chart", json!({"type": "bar", "at": "M30", "width": 200, "height": 100}));
    assert!(r.unshared.contains(&Unshared::Objects(0)));
    let r = one(&mut s, &tap, "formulas.defineName", json!({"name": "MyName", "refersTo": "=Budget!$C$5"}));
    assert!(r.unshared.contains(&Unshared::Names));
    let r = one(&mut s, &tap, "sheet.rename", json!({"sheet": 0, "name": "Renamed"}));
    assert!(r.unshared.contains(&Unshared::Sheets));
}

#[test]
fn every_registered_command_change_is_reported_or_unshared() {
    const SKIPPED: &[&str] = &[
        "file.new",
        "file.open",
        "file.close",
        "file.save",
        "file.saveAs",
        "file.exportCsv",
        "file.exportHtml",
        "file.exportPdf",
        "file.print",
        "window.activate",
        "window.close",
        "view.newWindow",
        "insert.picture",
        "pageLayout.background",
    ];
    for name in ["sales", "budget", "grades"] {
        let (mut s, tap) = sample(name);
        for spec in command_specs() {
            if SKIPPED.contains(&spec.id) {
                continue;
            }
            let json_of = |s: &Session| s.active().and_then(|d| serde_json::to_value(&*d.wb).ok()).map(without_default_lines);
            let uid = s.active().map(|d| d.uid);
            let was = json_of(&s);
            tap.take();
            let _ = s.execute(spec.id, json!({}));
            let heard = tap.take();
            assert!(heard.len() <= 1, "{name} {}: {} reports for one command", spec.id, heard.len());
            if s.active().map(|d| d.uid) != uid {
                continue;
            }
            let now = json_of(&s);
            if now != was {
                let what = fields_that_differ(was.as_ref(), now.as_ref());
                assert!(heard.first().is_some_and(|r| !r.is_empty()), "{name} {}: the workbook changed ({what}) but no report names it", spec.id);
            }
        }
    }
}

/// The workbook fields (and the sheet fields) whose JSON differs, for a failure message.
fn fields_that_differ(a: Option<&Json>, b: Option<&Json>) -> String {
    let (Some(Json::Object(a)), Some(Json::Object(b))) = (a, b) else { return "not objects".into() };
    let mut out = Vec::new();
    for (k, va) in a {
        if b.get(k) == Some(va) {
            continue;
        }
        if let (Some(Json::Array(xs)), Some(Json::Array(ys))) = (Some(va), b.get(k)) {
            for (x, y) in xs.iter().zip(ys) {
                if let (Json::Object(x), Json::Object(y)) = (x, y) {
                    out.extend(x.iter().filter(|(f, v)| y.get(*f) != Some(*v)).map(|(f, _)| format!("{k}.{f}")));
                }
            }
        }
        out.push(k.clone());
    }
    out.join(", ")
}

/// The workbook JSON with row and column entries that say nothing (the default `LineInfo`)
/// dropped: an entry like that is the same workbook as no entry.
fn without_default_lines(mut wb: Json) -> Json {
    let default = serde_json::to_value(gridcraft_engine::model::LineInfo::default()).unwrap_or(Json::Null);
    if let Some(Json::Array(sheets)) = wb.get_mut("sheets") {
        for sheet in sheets {
            for key in ["rows", "cols"] {
                if let Some(Json::Object(m)) = sheet.get_mut(key) {
                    m.retain(|_, v| *v != default);
                }
            }
        }
    }
    wb
}
