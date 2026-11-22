//! Live co-editing hooks of the engine.
//!
//! [`EditObserver`] (H1) hears one [`EditReport`] per top-level command that changed a workbook:
//! which cells, lines and sheet properties differ, which structural edits ran, and everything else
//! that differs but is not shared yet ([`Unshared`], the safety net of H8). With no observer
//! installed none of this runs, and a session behaves as it did before.

use std::collections::BTreeMap;
use std::sync::Arc;

use gridcraft_core::CellRef;
use gridcraft_formula::adjust::{Axis, Edit};
use gridcraft_model::{LineInfo, Sheet, StyleId, Workbook};
use serde_json::Value as Json;

use crate::Session;

/// Hears every top-level command that changed a document ([`Session::set_observer`]).
pub trait EditObserver: Send {
    fn edited(&mut self, report: EditReport);
}

/// What one top-level command changed in one document.
#[derive(Clone, Debug)]
pub struct EditReport {
    pub doc: u64,
    pub command: String,
    pub params: Json,
    pub before: Arc<Workbook>,
    pub after: Arc<Workbook>,
    /// Structural edits in order, by sheet index in `before`.
    pub edits: Vec<(usize, Edit)>,
    /// Cells whose content or style differ from `before` replayed through `edits` (H5).
    pub cells: Vec<(usize, CellRef)>,
    /// Rows/columns whose [`LineInfo`] differs (after replay).
    pub lines: Vec<(usize, Axis, u32)>,
    /// Sheets whose shared properties (default width/height) changed.
    pub sheet_props: Vec<usize>,
    /// Everything else that differs: not shared in M1 (the safety net, H8).
    pub unshared: Vec<Unshared>,
}

impl EditReport {
    /// Whether the report names no change at all.
    pub fn is_empty(&self) -> bool {
        self.edits.is_empty() && self.cells.is_empty() && self.lines.is_empty() && self.sheet_props.is_empty() && self.unshared.is_empty()
    }
}

/// A change M1 does not share. The index is the sheet's index in the workbook.
#[derive(Clone, Debug, PartialEq)]
pub enum Unshared {
    /// The list of sheets (added, removed, moved) or a sheet's name.
    Sheets,
    Merges(usize),
    CondFormats(usize),
    Validations(usize),
    Tables(usize),
    Filter(usize),
    Objects(usize),
    Comments(usize),
    Links(usize),
    Sparklines(usize),
    Pivots(usize),
    Freeze(usize),
    Print(usize),
    Protection(usize),
    SheetView(usize),
    Names,
    /// A workbook-level setting, by field name.
    Workbook(&'static str),
    /// An [`Edit`] other than an insert or delete of lines, or one whose effect on the sheet
    /// cannot be replayed yet: the sheet's cells are not compared.
    Structure(usize),
}

/// What the running top-level command has done to a document, for the observer.
#[derive(Clone, Debug, Default)]
pub(crate) struct Observed {
    /// Which top-level command this is: a group begun under another one is not this command's own.
    pub(crate) epoch: u64,
    /// Structural edits in order (the sheet's name, the edit), nested commands included.
    pub(crate) edits: Vec<(String, Edit)>,
    /// A whole older workbook was put back (Undo, Redo, a rolled-back group that began before the
    /// command). The edits do not describe it.
    pub(crate) restored: bool,
}

impl crate::DocState {
    /// Notes that the running command put an older workbook back.
    pub(crate) fn note_restore(&mut self) {
        if let Some(o) = self.observed.as_mut() {
            o.restored = true;
        }
    }
}

impl Session {
    /// Installs (or removes) the observer. While one is installed, every top-level command that
    /// changes a document is reported once, with the structural edits of the commands nested in it.
    pub fn set_observer(&mut self, o: Option<Box<dyn EditObserver>>) {
        self.observer = o;
        if self.observer.is_none() {
            for d in &mut self.docs {
                d.observed = None;
            }
        }
    }

    pub(crate) fn observing(&self) -> bool {
        self.observer.is_some()
    }

    /// Starts collecting structural edits on a document (a top-level command is about to run).
    pub(crate) fn observe_begin(&mut self, doc: u64) {
        self.observe_epoch = self.observe_epoch.wrapping_add(1);
        if let Some(d) = self.docs.iter_mut().find(|d| d.uid == doc) {
            d.observed = Some(Observed { epoch: self.observe_epoch, ..Observed::default() });
        }
    }

    /// Stops collecting and hands back what was collected.
    pub(crate) fn observe_take(&mut self, doc: u64) -> Observed {
        self.docs.iter_mut().find(|d| d.uid == doc).and_then(|d| d.observed.take()).unwrap_or_default()
    }

    /// Reports a finished top-level command to the observer when it changed the document.
    pub(crate) fn observe_end(&mut self, doc: u64, command: &str, params: &Json, before: Arc<Workbook>, seen: Observed) {
        let Some(after) = self.docs.iter().find(|d| d.uid == doc).map(|d| d.wb.clone()) else { return };
        if Arc::ptr_eq(&before, &after) {
            return;
        }
        let report = diff(doc, command, params, before, after, seen);
        if report.is_empty() {
            return;
        }
        // The observer is out of the session while it runs, so it cannot be reached re-entrantly.
        // A panic in the observer is the observer's: it is dropped, and the command's own cleanup
        // (history limits, group bookkeeping) still runs.
        if let Some(mut o) = self.observer.take() {
            let heard = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| o.edited(report)));
            if heard.is_ok() && self.observer.is_none() {
                self.observer = Some(o);
            } else if heard.is_err() {
                log::error!("the edit observer panicked and was removed");
            }
        }
    }
}

/// Where the replay of `edits` over a sheet of `before` (H5) will plug in: the sheet as the edits
/// leave it, or `None` while they cannot be replayed. G7 implements it for inserts and deletes.
fn replayed(_before: &Sheet, _edits: &[&Edit]) -> Option<Arc<Sheet>> {
    None
}

fn diff(doc: u64, command: &str, params: &Json, before: Arc<Workbook>, after: Arc<Workbook>, seen: Observed) -> EditReport {
    let Observed { edits: named, restored, .. } = seen;
    let mut r = EditReport {
        doc,
        command: command.to_string(),
        params: crate::cmd::journal_params(params),
        before: before.clone(),
        after: after.clone(),
        edits: Vec::new(),
        cells: Vec::new(),
        lines: Vec::new(),
        sheet_props: Vec::new(),
        unshared: Vec::new(),
    };
    for (name, e) in named {
        match before.sheet_index(&name) {
            Some(i) => r.edits.push((i, e)),
            // The sheet did not exist when the command began (or was renamed inside it).
            None => push_once(&mut r.unshared, Unshared::Sheets),
        }
    }
    let same_layout = before.sheets.len() == after.sheets.len() && before.sheets.iter().zip(&after.sheets).all(|(b, a)| b.uid == a.uid);
    if same_layout {
        for (i, (b, a)) in before.sheets.iter().zip(&after.sheets).enumerate() {
            if b.name != a.name {
                push_once(&mut r.unshared, Unshared::Sheets);
            }
            if Arc::ptr_eq(b, a) {
                continue;
            }
            let own: Vec<&Edit> = r.edits.iter().filter(|(s, _)| *s == i).map(|(_, e)| e).collect();
            let lines_move = own.iter().any(|e| matches!(e, Edit::Insert { .. } | Edit::Delete { .. }));
            if restored {
                // Which steps the restored workbook crosses is not known: every changed sheet
                // may have shifted lines, so none is compared position by position.
                sheet_diff(i, b, a, command, &mut r);
                push_once(&mut r.unshared, Unshared::Structure(i));
            } else if own.is_empty() {
                sheet_diff(i, b, a, command, &mut r);
            } else if !lines_move {
                // A move of cells: the cells are where they are, but it is not an insert or delete.
                sheet_diff(i, b, a, command, &mut r);
                push_once(&mut r.unshared, Unshared::Structure(i));
            } else {
                match replayed(b, &own) {
                    Some(replay) => sheet_diff(i, &replay, a, command, &mut r),
                    None => push_once(&mut r.unshared, Unshared::Structure(i)),
                }
            }
        }
    } else {
        push_once(&mut r.unshared, Unshared::Sheets);
        if restored {
            for i in 0..before.sheets.len() {
                push_once(&mut r.unshared, Unshared::Structure(i));
            }
        }
        // Sheets that edits touched are still named, so a receiver can tell structure from the rest.
        let touched: Vec<usize> = r.edits.iter().map(|(s, _)| *s).collect();
        for i in touched {
            push_once(&mut r.unshared, Unshared::Structure(i));
        }
    }
    workbook_diff(&before, &after, &mut r.unshared);
    r
}

/// Whether a cell differs in what a person wrote into it (its content or its style), not in a
/// result: the cached value of a formula or Python cell is whatever recalculation gave, and every
/// peer recalculates for itself.
fn authored(b: Option<&gridcraft_model::Cell>, a: Option<&gridcraft_model::Cell>) -> bool {
    match (b, a) {
        (Some(b), Some(a)) => {
            // Every field is named, so a new `Cell` field has to be classified here.
            let gridcraft_model::Cell { value, formula, style, py } = b;
            let computed = a.formula.is_some() || a.py.is_some();
            *formula != a.formula || *py != a.py || *style != a.style || (!computed && *value != a.value)
        }
        _ => true,
    }
}

/// Controls compared without the value a linked cell gives a slider (it follows the cell).
fn same_controls(b: &[gridcraft_model::FormControl], a: &[gridcraft_model::FormControl]) -> bool {
    // Only a linked control's value follows a cell; an unlinked one holds what a person set.
    b.len() == a.len()
        && b.iter().zip(a).all(|(x, y)| {
            let follows_cell = x.link.is_some() && y.link.is_some();
            gridcraft_model::FormControl { value: if follows_cell { y.value } else { x.value }, ..x.clone() } == *y
        })
}

fn push_once(v: &mut Vec<Unshared>, u: Unshared) {
    if !v.contains(&u) {
        v.push(u);
    }
}

/// `fit`: a row height the engine measured by itself (the row is not set by hand) is not an edit:
/// every peer fits its own rows, with its own fonts.
fn lines_diff(i: usize, axis: Axis, fit: bool, b: &BTreeMap<u32, LineInfo>, a: &BTreeMap<u32, LineInfo>, out: &mut Vec<(usize, Axis, u32)>) {
    let mut keys: Vec<u32> = b.keys().chain(a.keys()).copied().collect();
    keys.sort_unstable();
    keys.dedup();
    let default = LineInfo::default();
    for k in keys {
        let (x, y) = (b.get(&k).unwrap_or(&default), a.get(&k).unwrap_or(&default));
        if x == y {
            continue;
        }
        let measured = fit && axis == Axis::Rows && !x.custom && !y.custom && LineInfo { size: None, ..*x } == LineInfo { size: None, ..*y };
        if !measured {
            out.push((i, axis, k));
        }
    }
}

/// Compares sheet `b` (as it was, replayed through the structural edits) with `a`.
fn sheet_diff(i: usize, b: &Sheet, a: &Sheet, command: &str, r: &mut EditReport) {
    let mut cells: Vec<CellRef> = b.cells.diff(&a.cells).into_iter().filter(|c| authored(b.cell(*c), a.cell(*c))).collect();
    cells.sort_unstable();
    cells.dedup();
    r.cells.extend(cells.into_iter().map(|c| (i, c)));
    // Heights follow content on every peer; only an explicit AutoFit command is an edit.
    let fit = command != "home.autofitRowHeight";
    lines_diff(i, Axis::Rows, fit, &b.rows, &a.rows, &mut r.lines);
    lines_diff(i, Axis::Cols, false, &b.cols, &a.cols, &mut r.lines);
    if b.default_col_width != a.default_col_width || b.default_row_height != a.default_row_height {
        r.sheet_props.push(i);
    }
    // Every other field is named here, so a field added to `Sheet` stops this from compiling
    // until someone decides whether it is shared. Fields bound to `_` below are each person's own
    // view of the sheet (zoom, scroll, gridlines, headings, zeros, formula display, view mode,
    // active cell): never shared, so never reported and never put back.
    let Sheet {
        name: _,
        uid: _,
        file_id: _,
        cells: _,
        cell_pictures,
        spill: _,
        spill_ranges: _,
        cols: _,
        rows: _,
        default_col_width: _,
        default_row_height: _,
        merges,
        freeze,
        tab_color,
        visibility,
        show_gridlines: _,
        show_headings: _,
        show_formulas: _,
        show_zeros: _,
        show_outline_symbols: _,
        summary_below,
        summary_right,
        right_to_left,
        zoom: _,
        cond_formats,
        validations,
        tables,
        autofilter,
        comments,
        hyperlinks,
        charts,
        images,
        shapes,
        sparklines,
        slicers,
        controls,
        pivots,
        protection,
        edit_ranges,
        print,
        view_active: _,
        view_top_left: _,
        view: _,
        preserved,
    } = b;
    let u = &mut r.unshared;
    if *merges != a.merges {
        push_once(u, Unshared::Merges(i));
    }
    if *cond_formats != a.cond_formats {
        push_once(u, Unshared::CondFormats(i));
    }
    if *validations != a.validations {
        push_once(u, Unshared::Validations(i));
    }
    if *tables != a.tables {
        push_once(u, Unshared::Tables(i));
    }
    if *autofilter != a.autofilter {
        push_once(u, Unshared::Filter(i));
    }
    if *cell_pictures != a.cell_pictures
        || *charts != a.charts
        || *images != a.images
        || *shapes != a.shapes
        || *slicers != a.slicers
        || !same_controls(controls, &a.controls)
    {
        push_once(u, Unshared::Objects(i));
    }
    if *comments != a.comments {
        push_once(u, Unshared::Comments(i));
    }
    if *hyperlinks != a.hyperlinks {
        push_once(u, Unshared::Links(i));
    }
    if *sparklines != a.sparklines {
        push_once(u, Unshared::Sparklines(i));
    }
    if *pivots != a.pivots {
        push_once(u, Unshared::Pivots(i));
    }
    if *freeze != a.freeze {
        push_once(u, Unshared::Freeze(i));
    }
    if *print != a.print {
        push_once(u, Unshared::Print(i));
    }
    if *protection != a.protection || *edit_ranges != a.edit_ranges {
        push_once(u, Unshared::Protection(i));
    }
    if *tab_color != a.tab_color
        || *visibility != a.visibility
        || *summary_below != a.summary_below
        || *summary_right != a.summary_right
        || *right_to_left != a.right_to_left
        || *preserved != a.preserved
    {
        push_once(u, Unshared::SheetView(i));
    }
}

fn workbook_diff(b: &Workbook, a: &Workbook, u: &mut Vec<Unshared>) {
    // As for sheets: every field is named, so a new one has to be classified here.
    let Workbook {
        sheets: _,
        names,
        styles,
        theme,
        date_system,
        calc,
        show_objects: _,
        remove_personal_info,
        active_sheet: _,
        view: _,
        props,
        protected_structure,
        structure_password,
        custom_lists,
        cell_styles,
        phonetics,
        vba_project,
        csv_delimiter,
        macros,
        macros_read: _,
        macros_unread,
        preserved,
        source_dir: _,
        region: _,
        source_path: _,
    } = b;
    if *names != a.names {
        push_once(u, Unshared::Names);
    }
    // Styles are shared through the cells that use them; a style that changed in place is not.
    let kept = u32::try_from(styles.len()).unwrap_or(u32::MAX);
    if (0..kept).any(|n| styles.get(StyleId(n)) != a.styles.get(StyleId(n))) {
        push_once(u, Unshared::Workbook("styles"));
    }
    let checks: [(&'static str, bool); 12] = [
        ("theme", *theme != a.theme),
        ("date_system", *date_system != a.date_system),
        ("calc", *calc != a.calc),
        ("remove_personal_info", *remove_personal_info != a.remove_personal_info),
        ("props", *props != a.props),
        ("protected_structure", *protected_structure != a.protected_structure),
        ("structure_password", *structure_password != a.structure_password),
        ("custom_lists", *custom_lists != a.custom_lists),
        ("cell_styles", *cell_styles != a.cell_styles),
        ("phonetics", *phonetics != a.phonetics),
        ("vba_project", *vba_project != a.vba_project || *csv_delimiter != a.csv_delimiter),
        ("macros", *macros != a.macros || *macros_unread != a.macros_unread || *preserved != a.preserved),
    ];
    for (name, differs) in checks {
        if differs {
            push_once(u, Unshared::Workbook(name));
        }
    }
}
