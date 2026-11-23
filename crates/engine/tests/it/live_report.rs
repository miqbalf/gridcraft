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
    // A change in this very session is heard, so silence below means something.
    let heard = one(&mut s, &tap, "cell.set", json!({"cell": "C5", "input": "41"}));
    assert!(!heard.is_empty());
    s.execute("formulas.functions", json!({})).unwrap();
    s.execute("edit.copy", json!({"range": "C5"})).unwrap();
    // Typing what the cell already holds changes nothing.
    let now = s.active().and_then(|d| d.wb.sheets[0].cell(at("C5")).map(|c| c.value.clone())).unwrap();
    assert_eq!(now, gridcraft_engine::core::Value::Number(41.0));
    s.execute("cell.set", json!({"cell": "C5", "input": "41"})).unwrap();
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

/// Commands that change a workbook only in a person's own view (never shared, so an empty report
/// is right), by id. A command that changes the workbook with an empty report must be named here.
const LOCAL: &[&str] = &[
    "view.zoomToSelection",
    "formulas.showFormulas",
    "view.showZeros",
    "view.showOutlineSymbols",
    "view.showObjects",
    "view.showSheetTabs",
    "view.showHorizontalScrollBar",
    "view.showVerticalScrollBar",
    "view.gridlines",
    "view.headings",
    "view.pageLayout",
    "view.pageBreakPreview",
];

/// Commands run with parameters on every sample, besides every command with none.
fn extra() -> Vec<(&'static str, Json)> {
    vec![
        (
            "home.conditionalFormat",
            json!({"range": "B20:B22", "rule": {"type": "cellIs", "operator": "greater", "value": 1, "preset": "lightRedFillDarkRedText"}}),
        ),
        ("home.mergeCells", json!({"range": "B30:C30"})),
        ("view.freezePanes", json!({"cell": "B3"})),
        ("home.wrapText", json!({"range": "B2:B2", "on": true})),
        // Line edits inside a table: the header row follows (G7). `data.subtotal` is in the
        // registry above.
        ("insert.table", json!({"range": "N40:P44", "header": true})),
        ("home.insertColumns", json!({"cols": "O:O", "range": "O40"})),
        ("home.insertRows", json!({"rows": "42:42"})),
        ("home.deleteRows", json!({"rows": "40:40"})),
    ]
}

/// Workbook JSON keys (and sheet keys) that are each person's own view: a change there is not an edit.
const LOCAL_KEYS: &[&str] = &[
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

/// The report category a changed JSON key must produce, where the key names one.
fn category(key: &str) -> Option<fn(&Unshared) -> bool> {
    Some(match key {
        "merges" => |u| matches!(u, Unshared::Merges(_)),
        "cond_formats" => |u| matches!(u, Unshared::CondFormats(_)),
        "validations" => |u| matches!(u, Unshared::Validations(_)),
        "tables" => |u| matches!(u, Unshared::Tables(_)),
        "autofilter" => |u| matches!(u, Unshared::Filter(_)),
        "charts" | "images" | "shapes" | "slicers" | "cell_pictures" => |u| matches!(u, Unshared::Objects(_)),
        "comments" => |u| matches!(u, Unshared::Comments(_)),
        "hyperlinks" => |u| matches!(u, Unshared::Links(_)),
        "sparklines" => |u| matches!(u, Unshared::Sparklines(_)),
        "pivots" => |u| matches!(u, Unshared::Pivots(_)),
        "freeze" => |u| matches!(u, Unshared::Freeze(_)),
        "print" => |u| matches!(u, Unshared::Print(_)),
        "protection" | "edit_ranges" => |u| matches!(u, Unshared::Protection(_)),
        "names" => |u| matches!(u, Unshared::Names),
        _ => return None,
    })
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
    let (mut ran, mut changed_any, mut checked, mut replayed_reports) = (0usize, 0usize, 0usize, 0usize);
    let mut failures: Vec<String> = Vec::new();
    let mut local_used: std::collections::BTreeSet<&str> = Default::default();
    for name in ["sales", "budget", "grades"] {
        let (mut s, tap) = sample(name);
        // Every registered command with empty parameters, then commands that only change
        // something given parameters (so the checks of each kind of change are reached).
        let mut cases: Vec<(&str, Json)> = command_specs().iter().map(|c| (c.id, json!({}))).collect();
        cases.extend(extra());
        for (id, params) in cases {
            if SKIPPED.contains(&id) {
                continue;
            }
            let json_of = |s: &Session| s.active().and_then(|d| serde_json::to_value(&*d.wb).ok());
            let uid = s.active().map(|d| d.uid);
            let was = json_of(&s);
            tap.take();
            let _ = s.execute(id, params);
            ran += 1;
            let heard = tap.take();
            if heard.len() > 1 {
                failures.push(format!("{name} {}: {} reports for one command", id, heard.len()));
            }
            if s.active().map(|d| d.uid) != uid {
                continue;
            }
            let now = json_of(&s);
            if now == was {
                continue;
            }
            changed_any += 1;
            let (was, now) = (without_default_lines(was.unwrap_or(Json::Null)), without_default_lines(now.unwrap_or(Json::Null)));
            if was == now {
                // Only an entry that says nothing (a default row or column) was added: the same workbook.
                continue;
            }
            let (was, now) = (without_local(was), without_local(now));
            let what = fields_that_differ(Some(&was), Some(&now));
            let Some(report) = heard.first() else {
                // Nothing was reported: only a command named as local may change the workbook that way,
                // and then only its view.
                if LOCAL.contains(&id) {
                    local_used.insert(id);
                }
                if !(LOCAL.contains(&id) && was == now) {
                    failures.push(format!("{name} {}: the workbook changed ({what}) and the report is empty", id));
                }
                continue;
            };
            checked += 1;
            if report.is_empty() {
                failures.push(format!("{name} {}: the workbook changed ({what}) and the report names nothing", id));
            }
            // A change to a field with its own category must produce that category. For line
            // edits (G7) the baseline is `before` replayed through them with the public remote
            // API, one call per edit, the way a peer applies them; a report with any other edit
            // (a move, a restore) is marked Structure and is not compared field by field.
            let structure = report.unshared.iter().any(|u| matches!(u, Unshared::Structure(_)));
            if structure {
                continue;
            }
            let what = if report.edits.is_empty() {
                what
            } else {
                replayed_reports += 1;
                let Some(replayed) = replay_json(report) else {
                    failures.push(format!("{name} {id}: the line edits of the report cannot be replayed remotely"));
                    continue;
                };
                fields_that_differ(Some(&without_local(without_default_lines(replayed))), Some(&now))
            };
            for key in what.split(", ").filter(|k| !k.is_empty()) {
                let field = key.rsplit('.').next().unwrap_or(key);
                if let Some(is) = category(field)
                    && !report.unshared.iter().any(is)
                {
                    failures.push(format!("{name} {id}: `{key}` changed and the report has no matching Unshared: {:?}", report.unshared));
                }
            }
        }
    }
    assert!(failures.is_empty(), "{} problems:\n{}", failures.len(), failures.join("\n"));
    // Every command named as local was seen changing only a person's own view.
    let unused: Vec<_> = LOCAL.iter().filter(|id| !local_used.contains(*id)).collect();
    assert!(unused.is_empty(), "named as local but never seen changing only the view: {unused:?}");
    // The run must have exercised something: relations, not counts of our choosing.
    assert!(replayed_reports > 0, "no report with line edits went through the replay check");
    assert!(changed_any > 10, "only {changed_any} of {ran} commands changed a workbook");
    assert!(checked * 2 > changed_any, "{checked} of {changed_any} changes were checked against a report");
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

/// The workbook JSON without the keys that are each person's own view.
fn without_local(mut wb: Json) -> Json {
    if let Json::Object(m) = &mut wb {
        m.retain(|k, _| !LOCAL_KEYS.contains(&k.as_str()));
        if let Some(Json::Array(sheets)) = m.get_mut("sheets") {
            for sheet in sheets {
                if let Json::Object(m) = sheet {
                    m.retain(|k, _| !LOCAL_KEYS.contains(&k.as_str()));
                }
            }
        }
    }
    wb
}

/// The JSON of the report's `before` with its line edits applied through `apply_remote`, one call
/// per edit; `None` when an edit is not a line insert or delete, or a peer refuses it.
fn replay_json(report: &EditReport) -> Option<Json> {
    let mut peer = Session::new();
    peer.execute("file.new", json!({})).ok()?;
    let uid = peer.documents()[0].uid;
    peer.replace_workbook_quiet(uid, report.before.clone()).ok()?;
    peer.apply_remote(uid, |cx| {
        for (sheet, e) in &report.edits {
            match e {
                Edit::Insert { axis, at, count } => cx.insert_lines(*sheet, *axis, *at, *count)?,
                Edit::Delete { axis, at, count } => cx.delete_lines(*sheet, *axis, &[(*at, *at + *count - 1)])?,
                _ => return Err(gridcraft_engine::EngineError::Other("not a line edit".into())),
            }
        }
        Ok(())
    })
    .ok()?;
    serde_json::to_value(&*peer.documents()[0].wb).ok()
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

#[test]
fn one_changed_input_reports_one_cell() {
    let (mut s, tap) = sample("budget");
    s.execute("cell.set", json!({"cell": "Z1", "input": "1"})).unwrap();
    s.execute("range.fill", json!({"range": "Z2:Z1001", "input": "=$Z$1+ROW()"})).unwrap();
    let r = one(&mut s, &tap, "cell.set", json!({"cell": "Z1", "input": "5"}));
    assert_eq!(r.cells, vec![(0, at("Z1"))], "the 1000 dependents are recalculated by every peer, not written");
    // They did change in the workbook itself.
    let (b, a) = (r.before.sheets[0].cell(at("Z500")).map(|c| c.value.clone()), r.after.sheets[0].cell(at("Z500")).map(|c| c.value.clone()));
    assert_ne!(b, a);
    assert!(r.lines.is_empty() && r.unshared.is_empty() && r.edits.is_empty() && r.sheet_props.is_empty());
}

#[test]
fn a_volatile_cell_is_not_an_edit() {
    let (mut s, tap) = sample("budget");
    s.execute("cell.set", json!({"cell": "Z1", "input": "=RAND()"})).unwrap();
    let r = one(&mut s, &tap, "cell.set", json!({"cell": "Z2", "input": "1"}));
    assert_eq!(r.cells, vec![(0, at("Z2"))]);
}

#[test]
fn a_linked_control_value_is_not_an_object_change() {
    let (mut s, tap) = sample("budget");
    s.execute("insert.slider", json!({"at": "J30", "link": "$Z$1", "min": 0, "max": 100})).unwrap();
    let r = one(&mut s, &tap, "cell.set", json!({"cell": "Z1", "input": "40"}));
    assert!(!r.unshared.contains(&Unshared::Objects(0)), "the control follows its cell on every peer: {:?}", r.unshared);
}

#[test]
fn per_user_view_state_reports_nothing() {
    let (mut s, tap) = sample("budget");
    for (id, params) in [("view.zoom", json!({"percent": 150})), ("formulas.showFormulas", json!({"on": true}))] {
        tap.take();
        let before = s.active().map(|d| d.wb.clone()).unwrap();
        s.execute(id, params).unwrap();
        let after = s.active().map(|d| d.wb.clone()).unwrap();
        assert!(!Arc::ptr_eq(&before, &after), "{id} does change the workbook");
        assert!(tap.take().is_empty(), "{id} is each person's own and must not be reported, let alone put back");
    }
}

#[test]
fn a_rolled_back_dry_run_leaves_no_edits() {
    // An empty sheet, so the dry run finds no data to write over and the macro goes ahead.
    let mut s = Session::new();
    s.new_workbook();
    let tap = Tap::default();
    s.set_observer(Some(Box::new(tap.clone())));
    s.execute(
        "macro.save",
        json!({"macro": {"name": "Grow", "steps": [{"type": "command", "id": "home.insertRows", "params": {"rows": "3:3"}}], "output": {"onConflict": "ask"}}}),
    )
    .unwrap();
    tap.take();
    s.execute("macro.run", json!({"name": "Grow"})).unwrap();
    let all = tap.take();
    assert_eq!(all.len(), 1);
    let inserts = all[0].edits.iter().filter(|(_, e)| matches!(e, Edit::Insert { .. })).count();
    assert_eq!(inserts, 1, "the macro inserts once; its dry run (put back) is not an edit: {:?}", all[0].edits);
}

#[test]
fn undo_across_a_structural_step_is_marked_structure() {
    let (mut s, tap) = sample("budget");
    one(&mut s, &tap, "home.insertRows", json!({"rows": "3:3"}));
    let r = one(&mut s, &tap, "edit.undo", json!({}));
    assert!(r.unshared.contains(&Unshared::Structure(0)), "{:?}", r.unshared);
    let r = one(&mut s, &tap, "edit.redo", json!({}));
    assert!(r.unshared.contains(&Unshared::Structure(0)), "{:?}", r.unshared);
}

#[test]
fn a_rolled_back_group_that_began_earlier_is_marked_structure() {
    let (mut s, tap) = sample("budget");
    s.execute(
        "macro.save",
        json!({"macro": {"name": "Grow", "steps": [
            {"type": "command", "id": "home.insertRows", "params": {"rows": "3:3"}},
            {"type": "command", "id": "home.insertRows", "params": {"rows": "3:3"}}
        ], "output": {"onConflict": "overwrite"}}}),
    )
    .unwrap();
    s.execute("macro.debug", json!({"action": "start", "name": "Grow"})).unwrap();
    let stepped = s.execute("macro.debug", json!({"action": "step"})).unwrap();
    tap.take();
    // Stopping without keeping rolls back the run's group, which began in an earlier command.
    s.execute("macro.debug", json!({"action": "stop"})).unwrap();
    let heard = tap.take();
    assert_eq!(heard.len(), 1, "the rollback changed the workbook; step said {stepped}");
    assert!(heard[0].unshared.contains(&Unshared::Structure(0)), "{:?}", heard[0].unshared);
}

#[test]
fn cut_and_paste_is_a_move_edit() {
    let (mut s, tap) = sample("budget");
    s.execute("edit.cut", json!({"range": "C5:C6"})).unwrap();
    let r = one(&mut s, &tap, "edit.paste", json!({"at": "M40"}));
    assert!(r.edits.iter().any(|(_, e)| matches!(e, Edit::Move { .. })), "{:?}", r.edits);
    assert!(r.unshared.contains(&Unshared::Structure(0)));
    assert!(r.cells.contains(&(0, at("M40"))), "the moved cells are still reported");
}

#[test]
fn installing_a_filter_is_a_reported_command() {
    let (mut s, tap) = sample("budget");
    let r = one(&mut s, &tap, "data.filter", json!({"range": "B4:F14", "install": true}));
    assert!(r.unshared.contains(&Unshared::Filter(0)));
}

/// An observer that panics on its first report.
struct Boom;
impl EditObserver for Boom {
    fn edited(&mut self, _: EditReport) {
        std::panic::resume_unwind(Box::new("observer panic"));
    }
}

#[test]
fn a_panicking_observer_does_not_skip_the_commands_cleanup() {
    let mut s = Session::new();
    s.execute("file.new", json!({"sample": "budget"})).unwrap();
    s.set_observer(Some(Box::new(Boom)));
    let undo_before = s.active().map(|d| d.undo.len()).unwrap();
    s.execute("cell.set", json!({"cell": "C5", "input": "9"})).unwrap();
    assert_eq!(s.active().map(|d| d.undo.len()), Some(undo_before + 1), "the step was kept");
    assert_eq!(s.depth(), 0);
    // The observer is gone; commands go on as for a single user.
    s.execute("cell.set", json!({"cell": "C6", "input": "9"})).unwrap();
    s.execute("edit.undo", json!({})).unwrap();
}

#[test]
fn a_row_the_engine_fits_by_itself_is_not_a_line_edit() {
    let (mut s, tap) = sample("budget");
    s.execute("cell.set", json!({"cell": "B30", "input": "a long text that wraps over several lines of a narrow column"})).unwrap();
    s.execute("home.columnWidth", json!({"cols": "B:B", "width": 40})).unwrap();
    let r = one(&mut s, &tap, "home.wrapText", json!({"range": "B30", "on": true}));
    let (b, a) = (r.before.sheets[0].rows.get(&29).and_then(|l| l.size), r.after.sheets[0].rows.get(&29).and_then(|l| l.size));
    assert_ne!(b, a, "the row grew to fit the wrapped text");
    assert!(r.lines.is_empty(), "every peer fits its own rows: {:?}", r.lines);
    assert!(r.fitted.contains(&(0, 29)), "the fitted row is named apart: {:?}", r.fitted);
    assert!(r.cells.contains(&(0, at("B30"))));
}

#[test]
fn an_unlinked_control_value_set_by_hand_is_an_object_change() {
    let (mut s, tap) = sample("budget");
    let made = s.execute("insert.slider", json!({"at": "J30", "min": 0, "max": 100})).unwrap();
    let id = made["control"].clone();
    tap.take();
    let r = one(&mut s, &tap, "control.setValue", json!({"id": id, "value": 30}));
    assert!(r.unshared.contains(&Unshared::Objects(0)), "{:?}", r.unshared);
    let r = one(&mut s, &tap, "control.format", json!({"id": id, "value": 60}));
    assert!(r.unshared.contains(&Unshared::Objects(0)), "{:?}", r.unshared);
}

#[test]
fn a_nested_panic_takes_its_logged_edits_back() {
    gridcraft_engine::register_test_command("test.g3InsertThenPanic", |s, _| {
        s.execute("home.insertRows", json!({"rows": "3:3"}))?;
        std::panic::resume_unwind(Box::new("boom"))
    });
    gridcraft_engine::register_test_command("test.g3Outer", |s, _| {
        // The inner command inserts a row, then panics; `execute` restores the workbook.
        let inner = s.execute("test.g3InsertThenPanic", json!({}));
        assert!(inner.is_err());
        s.execute("cell.set", json!({"cell": "Z1", "input": "1"}))
    });
    let (mut s, tap) = sample("budget");
    let r = one(&mut s, &tap, "test.g3Outer", json!({}));
    assert!(r.edits.is_empty(), "the insert was undone by the panic, so it is not an edit: {:?}", r.edits);
    assert!(r.cells.contains(&(0, at("Z1"))));
}
