//! Live co-editing hooks of the engine.
//!
//! [`EditObserver`] (H1) hears one [`EditReport`] per top-level command that changed a workbook:
//! which cells, lines and sheet properties differ, which structural edits ran, and everything else
//! that differs but is not shared yet ([`Unshared`], the safety net of H8). With no observer
//! installed none of this runs, and a session behaves as it did before.

use std::collections::BTreeMap;
use std::sync::Arc;

use gridcraft_calc::Key;
use gridcraft_core::{CellRef, MAX_COLS, MAX_ROWS, RangeRef};
use gridcraft_formula::adjust::{Axis, Edit};
use gridcraft_model::{Cell, DEFAULT_COL_WIDTH, DEFAULT_ROW_HEIGHT, LineInfo, Sheet, Style, StyleId, Workbook};
use serde_json::Value as Json;

use crate::{EngineError, Result, Selection, Session};

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
    /// Rows whose height the engine fitted to their content by itself (not edits, so not in
    /// `lines`), by sheet index. A live mapping may still share the height it measured: a peer
    /// that does not fit the row would otherwise keep the old one.
    pub fitted: Vec<(usize, u32)>,
    /// Sheets whose shared properties (default width/height) changed.
    pub sheet_props: Vec<usize>,
    /// Everything else that differs: not shared in M1 (the safety net, H8).
    pub unshared: Vec<Unshared>,
}

impl EditReport {
    /// Whether the report names no change at all.
    pub fn is_empty(&self) -> bool {
        self.edits.is_empty()
            && self.cells.is_empty()
            && self.lines.is_empty()
            && self.fitted.is_empty()
            && self.sheet_props.is_empty()
            && self.unshared.is_empty()
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
    /// An [`Edit`] other than an insert or delete of lines (a move of cells, a restored workbook),
    /// or edits that cannot be replayed: the sheet's cells are not compared position by position.
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
    /// Takes back the structural edits logged after the first `len` (when a length is given).
    pub(crate) fn observe_truncate(&mut self, len: Option<usize>) {
        if let (Some(o), Some(len)) = (self.observed.as_mut(), len) {
            o.edits.truncate(len);
        }
    }

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

    /// How many structural edits the observed command on `doc` has logged so far.
    pub(crate) fn observed_len(&self, doc: Option<u64>) -> Option<usize> {
        let d = self.docs.iter().find(|d| Some(d.uid) == doc)?;
        d.observed.as_ref().map(|o| o.edits.len())
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

/// `before` with every structural edit of the command replayed by position (H5), or `None` when
/// the command did something else as well (a move of cells, a restored workbook) or an edit
/// cannot be replayed. The whole workbook is replayed, not one sheet: an insert also rewrites
/// formulas, names and charts on other sheets, and those are no change of their own.
fn replay_lines(before: &Workbook, edits: &[(usize, Edit)]) -> Option<Workbook> {
    if edits.is_empty() {
        return None;
    }
    let mut wb = before.clone();
    for (sheet, e) in edits {
        match e {
            Edit::Insert { axis, at, count } => {
                crate::cmd::sheet::insert_lines_in(&mut wb, *sheet, *axis, *at, *count).ok()?;
            }
            Edit::Delete { axis, at, count } => {
                crate::cmd::sheet::delete_lines_in(&mut wb, *sheet, *axis, &[(*at, *count)]).ok()?;
            }
            _ => return None,
        }
    }
    Some(wb)
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
        fitted: Vec::new(),
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
    let whole = if same_layout && !restored { replay_lines(&before, &r.edits) } else { None };
    if let Some(replay) = &whole {
        // Every sheet is compared with the replay, so what the edits moved is not reported.
        for (i, (b, a)) in replay.sheets.iter().zip(&after.sheets).enumerate() {
            if b.name != a.name {
                push_once(&mut r.unshared, Unshared::Sheets);
            }
            if !Arc::ptr_eq(b, a) {
                sheet_diff(i, b, a, command, &mut r);
            }
        }
        workbook_diff(replay, &after, &mut r.unshared);
        return r;
    }
    if same_layout {
        for (i, (b, a)) in before.sheets.iter().zip(&after.sheets).enumerate() {
            if b.name != a.name {
                push_once(&mut r.unshared, Unshared::Sheets);
            }
            if Arc::ptr_eq(b, a) {
                continue;
            }
            let own: Vec<&Edit> = r.edits.iter().filter(|(s, _)| *s == i).map(|(_, e)| e).collect();
            if restored {
                // Which steps the restored workbook crosses is not known: every changed sheet
                // may have shifted lines, so none is compared position by position.
                sheet_diff(i, b, a, command, &mut r);
                push_once(&mut r.unshared, Unshared::Structure(i));
            } else if own.is_empty() {
                sheet_diff(i, b, a, command, &mut r);
            } else {
                // A move of cells, or edits that cannot be replayed together: the cells are where
                // they are, but the edits are not replayed position by position.
                sheet_diff(i, b, a, command, &mut r);
                push_once(&mut r.unshared, Unshared::Structure(i));
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

/// `fit`: a row height the engine measured by itself (the row is not set by hand) is not a line
/// edit (no "set by hand" height): such rows go to `fitted` instead, so an observer can still
/// share the height its author measured (peers that apply a remote change do not fit rows).
fn lines_diff(i: usize, axis: Axis, fit: bool, b: &BTreeMap<u32, LineInfo>, a: &BTreeMap<u32, LineInfo>, r: &mut EditReport) {
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
        if measured {
            r.fitted.push((i, k));
        } else {
            r.lines.push((i, axis, k));
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
    lines_diff(i, Axis::Rows, fit, &b.rows, &a.rows, r);
    lines_diff(i, Axis::Cols, false, &b.cols, &a.cols, r);
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

/// Takes over Undo and Redo of a live session (H3): `true` for a redo; answers whether it did
/// anything.
pub(crate) type UndoHook = Box<dyn FnMut(bool) -> bool + Send>;

/// The live undo hook, bound to the document that was active when it was set. `hook` is `None`
/// once it panicked: undo and redo of that document then do nothing (snapshot undo would take
/// other peers' cells back) until a hook is set again.
pub(crate) struct LiveUndo {
    pub(crate) doc: u64,
    hook: Option<UndoHook>,
}

fn refuse<T>(msg: impl Into<String>) -> Result<T> {
    Err(EngineError::Other(msg.into()))
}

/// Changes that arrive from other peers (H2), applied to a copy of the workbook that
/// [`Session::apply_remote`] puts in place only when all of them succeed. Nothing here checks
/// what the local user may edit: a peer's change was allowed where it was made.
pub struct RemoteCtx<'a> {
    wb: Workbook,
    changed: Vec<Key>,
    /// Lines or sizes changed: every formula is recalculated.
    recalc_all: bool,
    _session: std::marker::PhantomData<&'a mut Session>,
}

impl RemoteCtx<'_> {
    pub fn workbook(&self) -> &Workbook {
        &self.wb
    }

    /// Puts `cell` at `at` (`None` clears it). The cell and its dependents are recalculated.
    pub fn set_cell(&mut self, sheet: usize, at: CellRef, cell: Option<Cell>) -> Result<()> {
        if !at.is_valid() {
            return refuse("a remote cell lies outside the grid");
        }
        if cell.as_ref().is_some_and(|c| usize::try_from(c.style.0).map_or(true, |n| n >= self.wb.styles.len())) {
            return refuse("a remote cell names a style that does not exist");
        }
        let Some(sh) = self.wb.sheet_mut(sheet) else { return refuse("a remote change names a sheet that does not exist") };
        match cell {
            Some(c) => sh.set_cell(at, c),
            None => {
                sh.remove_cell(at);
            }
        }
        self.changed.push((sheet, at));
        Ok(())
    }

    /// Sets (or with `None` removes) the information of one row or column.
    pub fn set_line(&mut self, sheet: usize, axis: Axis, index: u32, info: Option<LineInfo>) -> Result<()> {
        let limit = if axis == Axis::Rows { MAX_ROWS } else { MAX_COLS };
        if index >= limit {
            return refuse("a remote line lies outside the grid");
        }
        if let Some(i) = &info {
            if i.size.is_some_and(|s| !s.is_finite() || s < 0.0) {
                return refuse("a remote line has an invalid size");
            }
            if i.style.is_some_and(|s| usize::try_from(s.0).map_or(true, |n| n >= self.wb.styles.len())) {
                return refuse("a remote line names a style that does not exist");
            }
        }
        let Some(sh) = self.wb.sheet_mut(sheet) else { return refuse("a remote change names a sheet that does not exist") };
        let lines = if axis == Axis::Rows { &mut sh.rows } else { &mut sh.cols };
        match info {
            Some(i) => {
                lines.insert(index, i);
            }
            None => {
                lines.remove(&index);
            }
        }
        self.recalc_all = true;
        Ok(())
    }

    /// Sets (or with `None` resets) the default row height or column width.
    pub fn set_default_size(&mut self, sheet: usize, axis: Axis, size: Option<f32>) -> Result<()> {
        if size.is_some_and(|s| !s.is_finite() || s <= 0.0) {
            return refuse("a remote default size is invalid");
        }
        let Some(sh) = self.wb.sheet_mut(sheet) else { return refuse("a remote change names a sheet that does not exist") };
        if axis == Axis::Rows {
            sh.default_row_height = size.unwrap_or(DEFAULT_ROW_HEIGHT);
        } else {
            sh.default_col_width = size.unwrap_or(DEFAULT_COL_WIDTH);
        }
        self.recalc_all = true;
        Ok(())
    }

    /// The id of `style` in the workbook's table; equal formats give equal ids.
    pub fn intern_style(&mut self, style: &Style) -> StyleId {
        self.wb.styles.intern(style.clone())
    }

    pub fn style_of(&self, id: StyleId) -> Option<&Style> {
        usize::try_from(id.0).ok().filter(|n| *n < self.wb.styles.len()).map(|_| self.wb.styles.get(id))
    }

    /// Inserts `count` lines before `at` (H5): cells, line information, the sheet's features and
    /// every reference in the workbook move as for a local insert, with no undo step. Refused
    /// (nothing changes) past the end of the grid.
    pub fn insert_lines(&mut self, sheet: usize, axis: Axis, at: u32, count: u32) -> Result<()> {
        let mut wb = self.wb.clone();
        crate::cmd::sheet::insert_lines_in(&mut wb, sheet, axis, at, count)?;
        self.wb = wb;
        self.recalc_all = true;
        Ok(())
    }

    /// Deletes the inclusive `(first, last)` areas of lines, in any order (overlapping or
    /// touching areas are merged). All areas are positions in one coordinate frame, the one
    /// before the delete: never batch sequential deletes (two `Delete` edits of one report, the
    /// second counted after the first) into one call, or they merge into a single area; make one
    /// call per edit. Refused (nothing changes) outside the grid.
    pub fn delete_lines(&mut self, sheet: usize, axis: Axis, areas: &[(u32, u32)]) -> Result<()> {
        let limit = if axis == Axis::Rows { MAX_ROWS } else { MAX_COLS };
        let mut spans: Vec<(u32, u32)> = Vec::with_capacity(areas.len());
        for &(a, b) in areas {
            let (a, b) = (a.min(b), a.max(b));
            if b >= limit {
                return refuse("a remote delete names lines outside the grid");
            }
            spans.push((a, b));
        }
        spans.sort_unstable();
        let mut merged: Vec<(u32, u32)> = Vec::new();
        for (a, b) in spans {
            match merged.last_mut() {
                Some(last) if a <= last.1.saturating_add(1) => last.1 = last.1.max(b),
                _ => merged.push((a, b)),
            }
        }
        // Last area first, as the local command applies them.
        let areas: Vec<(u32, u32)> = merged.into_iter().rev().map(|(a, b)| (a, b - a + 1)).collect();
        if areas.is_empty() {
            return Ok(());
        }
        let mut wb = self.wb.clone();
        crate::cmd::sheet::delete_lines_in(&mut wb, sheet, axis, &areas)?;
        self.wb = wb;
        self.recalc_all = true;
        Ok(())
    }
}

impl Session {
    /// Applies changes from other peers to document `doc`. Callers apply one call per received
    /// update or batch (each call copies the workbook and recalculates). While an undo group is
    /// open on `doc` ([`Session::in_group`]) this refuses with [`EngineError::RemoteBusy`], because
    /// the group's rollback would take the change back: buffer the update and retry after the
    /// group ends. No undo step, no journal entry, no
    /// observer call; the changed cells are recalculated (everything after lines), and the
    /// selection stays. An `Err` leaves the workbook exactly as it was.
    pub fn apply_remote<R>(&mut self, doc: u64, f: impl FnOnce(&mut RemoteCtx) -> Result<R>) -> Result<R> {
        if self.in_group(doc) {
            return Err(EngineError::RemoteBusy);
        }
        let Some(d) = self.docs.iter_mut().find(|d| d.uid == doc) else { return Err(EngineError::NoDocument) };
        let mut cx = RemoteCtx { wb: (*d.wb).clone(), changed: Vec::new(), recalc_all: false, _session: std::marker::PhantomData };
        let r = f(&mut cx)?;
        let RemoteCtx { mut wb, changed, recalc_all, .. } = cx;
        if recalc_all {
            d.calc.recalc_all(&mut wb);
        } else if !changed.is_empty() {
            d.calc.cells_changed(&mut wb, &changed);
        }
        crate::cmd::controls::sync_linked_values(&mut wb);
        d.wb = Arc::new(wb);
        d.revision += 1;
        Ok(r)
    }

    /// Puts `wb` in place of the document's workbook without an undo step or an observer call, and
    /// recalculates everything. The selection stays where it can.
    ///
    /// Refuses with [`EngineError::RemoteBusy`] while an undo group is open on `doc`, like
    /// [`Session::apply_remote`]: the group's rollback would put the old workbook back.
    pub fn replace_workbook_quiet(&mut self, doc: u64, wb: Arc<Workbook>) -> Result<()> {
        if self.in_group(doc) {
            return Err(EngineError::RemoteBusy);
        }
        let Some(d) = self.docs.iter_mut().find(|d| d.uid == doc) else { return Err(EngineError::NoDocument) };
        if wb.sheets.is_empty() {
            return refuse("a workbook needs at least one sheet");
        }
        let mut fresh = (*wb).clone();
        d.calc.rebuild(&fresh);
        d.calc.recalc_all(&mut fresh);
        d.wb = Arc::new(fresh);
        d.revision += 1;
        Ok(())
    }

    /// Moves the selection without a command (a collaborator's cursor never reaches here; this is
    /// for the local view after a remote change). Clamped to the grid, never empty.
    pub fn set_selection_quiet(&mut self, doc: u64, sel: Selection) -> Result<()> {
        let Some(d) = self.docs.iter_mut().find(|d| d.uid == doc) else { return Err(EngineError::NoDocument) };
        let clamp = |c: CellRef| CellRef::new(c.row.min(MAX_ROWS - 1), c.col.min(MAX_COLS - 1));
        let active = clamp(sel.active);
        let mut ranges: Vec<RangeRef> = sel.ranges.iter().map(|r| RangeRef::new(clamp(r.start), clamp(r.end))).collect();
        if ranges.last().is_none_or(|r| !r.contains(active)) {
            ranges.push(RangeRef::cell(active));
        }
        d.selection = Selection { active, anchor: clamp(sel.anchor), ranges };
        Ok(())
    }

    /// Installs (or removes) the live undo hook (H3), bound to the document that is active now.
    /// While set, `edit.undo` and `edit.redo` on that document call it instead of the snapshot
    /// history, which stays as it is; other documents keep their own undo. If the hook panics
    /// (native builds; on wasm a panic aborts), undo and redo of the document do nothing until a
    /// hook is set again, see [`Session::live_undo_failed`].
    pub fn set_live_undo_hook(&mut self, hook: Option<Box<dyn FnMut(bool) -> bool + Send>>) {
        let doc = self.active().map_or(0, |d| d.uid);
        self.live_undo = hook.map(|h| LiveUndo { doc, hook: Some(h) });
    }

    /// [`Session::set_live_undo_hook`] bound to document `doc` instead of the active one. Refuses
    /// an unknown document (`NoDocument`); `None` removes the hook.
    pub fn set_live_undo_hook_for(&mut self, doc: u64, hook: Option<Box<dyn FnMut(bool) -> bool + Send>>) -> Result<()> {
        if !self.docs.iter().any(|d| d.uid == doc) {
            return Err(EngineError::NoDocument);
        }
        self.live_undo = hook.map(|h| LiveUndo { doc, hook: Some(h) });
        Ok(())
    }

    /// The document the live undo hook is bound to (also after the hook panicked).
    pub fn live_undo_doc(&self) -> Option<u64> {
        self.live_undo.as_ref().map(|l| l.doc)
    }

    /// Whether the live undo hook panicked and was dropped.
    pub fn live_undo_failed(&self) -> bool {
        self.live_undo.as_ref().is_some_and(|l| l.hook.is_none())
    }

    /// Whether the active document's undo belongs to the live hook.
    pub(crate) fn has_live_undo(&self) -> bool {
        self.live_undo.as_ref().is_some_and(|l| self.active().is_some_and(|d| d.uid == l.doc))
    }

    /// Runs the hook for the active document; `None` when its undo is not live.
    pub(crate) fn live_undo(&mut self, redo: bool) -> Option<bool> {
        if !self.has_live_undo() {
            return None;
        }
        let live = self.live_undo.as_mut()?;
        let Some(hook) = live.hook.as_mut() else { return Some(false) };
        let done = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| hook(redo)));
        match done {
            Ok(done) => Some(done),
            Err(_) => {
                live.hook = None;
                log::error!("the live undo hook panicked: undo and redo are off until a hook is set again");
                Some(false)
            }
        }
    }
}
