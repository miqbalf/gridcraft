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

impl Session {
    /// Installs (or removes) the observer. While one is installed, every top-level command that
    /// changes a document is reported once, with the structural edits of the commands nested in it.
    pub fn set_observer(&mut self, o: Option<Box<dyn EditObserver>>) {
        self.observer = o;
        if self.observer.is_none() {
            for d in &mut self.docs {
                d.observed_edits = None;
            }
        }
    }

    pub(crate) fn observing(&self) -> bool {
        self.observer.is_some()
    }

    /// Starts collecting structural edits on a document (a top-level command is about to run).
    pub(crate) fn observe_begin(&mut self, doc: u64) {
        if let Some(d) = self.docs.iter_mut().find(|d| d.uid == doc) {
            d.observed_edits = Some(Vec::new());
        }
    }

    /// Stops collecting and hands back what was collected.
    pub(crate) fn observe_take(&mut self, doc: u64) -> Vec<(String, Edit)> {
        self.docs.iter_mut().find(|d| d.uid == doc).and_then(|d| d.observed_edits.take()).unwrap_or_default()
    }

    /// Reports a finished top-level command to the observer when it changed the document.
    pub(crate) fn observe_end(&mut self, doc: u64, command: &str, params: &Json, before: Arc<Workbook>, edits: Vec<(String, Edit)>) {
        let Some(after) = self.docs.iter().find(|d| d.uid == doc).map(|d| d.wb.clone()) else { return };
        if Arc::ptr_eq(&before, &after) {
            return;
        }
        let report = diff(doc, command, params, before, after, edits);
        if report.is_empty() {
            return;
        }
        // The observer is out of the session while it runs, so it cannot be reached re-entrantly.
        if let Some(mut o) = self.observer.take() {
            o.edited(report);
            if self.observer.is_none() {
                self.observer = Some(o);
            }
        }
    }
}

/// Where the replay of `edits` over a sheet of `before` (H5) will plug in: the sheet as the edits
/// leave it, or `None` while they cannot be replayed. G7 implements it for inserts and deletes.
fn replayed(_before: &Sheet, _edits: &[&Edit]) -> Option<Arc<Sheet>> {
    None
}

fn diff(doc: u64, command: &str, params: &Json, before: Arc<Workbook>, after: Arc<Workbook>, named: Vec<(String, Edit)>) -> EditReport {
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
            if own.is_empty() {
                sheet_diff(i, b, a, &mut r);
            } else {
                match replayed(b, &own) {
                    Some(replay) => sheet_diff(i, &replay, a, &mut r),
                    None => push_once(&mut r.unshared, Unshared::Structure(i)),
                }
            }
        }
    } else {
        push_once(&mut r.unshared, Unshared::Sheets);
        // Sheets that edits touched are still named, so a receiver can tell structure from the rest.
        let touched: Vec<usize> = r.edits.iter().map(|(s, _)| *s).collect();
        for i in touched {
            push_once(&mut r.unshared, Unshared::Structure(i));
        }
    }
    workbook_diff(&before, &after, &mut r.unshared);
    r
}

fn push_once(v: &mut Vec<Unshared>, u: Unshared) {
    if !v.contains(&u) {
        v.push(u);
    }
}

fn lines_diff(i: usize, axis: Axis, b: &BTreeMap<u32, LineInfo>, a: &BTreeMap<u32, LineInfo>, out: &mut Vec<(usize, Axis, u32)>) {
    let mut keys: Vec<u32> = b.keys().chain(a.keys()).copied().collect();
    keys.sort_unstable();
    keys.dedup();
    let default = LineInfo::default();
    for k in keys {
        if b.get(&k).unwrap_or(&default) != a.get(&k).unwrap_or(&default) {
            out.push((i, axis, k));
        }
    }
}

/// Compares sheet `b` (as it was, replayed through the structural edits) with `a`.
fn sheet_diff(i: usize, b: &Sheet, a: &Sheet, r: &mut EditReport) {
    let mut cells: Vec<CellRef> = b.cells.diff(&a.cells);
    cells.sort_unstable();
    cells.dedup();
    r.cells.extend(cells.into_iter().map(|c| (i, c)));
    lines_diff(i, Axis::Rows, &b.rows, &a.rows, &mut r.lines);
    lines_diff(i, Axis::Cols, &b.cols, &a.cols, &mut r.lines);
    if b.default_col_width != a.default_col_width || b.default_row_height != a.default_row_height {
        r.sheet_props.push(i);
    }
    // Every other field is named here, so a field added to `Sheet` stops this from compiling
    // until someone decides whether it is shared.
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
        show_gridlines,
        show_headings,
        show_formulas,
        show_zeros,
        show_outline_symbols,
        summary_below,
        summary_right,
        right_to_left,
        zoom,
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
        view_active,
        view_top_left,
        view,
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
        || *controls != a.controls
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
        || *show_gridlines != a.show_gridlines
        || *show_headings != a.show_headings
        || *show_formulas != a.show_formulas
        || *show_zeros != a.show_zeros
        || *show_outline_symbols != a.show_outline_symbols
        || *summary_below != a.summary_below
        || *summary_right != a.summary_right
        || *right_to_left != a.right_to_left
        || *zoom != a.zoom
        || *view_active != a.view_active
        || *view_top_left != a.view_top_left
        || *view != a.view
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
        show_objects,
        remove_personal_info,
        active_sheet,
        view,
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
    let checks: [(&'static str, bool); 15] = [
        ("theme", *theme != a.theme),
        ("date_system", *date_system != a.date_system),
        ("calc", *calc != a.calc),
        ("show_objects", *show_objects != a.show_objects),
        ("remove_personal_info", *remove_personal_info != a.remove_personal_info),
        ("active_sheet", active_sheet.get() != a.active_sheet.get()),
        ("view", *view != a.view),
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
