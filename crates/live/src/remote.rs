//! The room's state into an engine workbook: what a remote update names, or everything, applied
//! to a [`RemoteCtx`] in one batch.
//!
//! Every target is recomputed from the replica's current state (never from a delta), so applying
//! a target twice, or in a different order, gives the same workbook: a cell is the base file's
//! cell unless the overlay says otherwise, and its content and its format are read separately.

use std::collections::BTreeSet;
use std::sync::Arc;

use gridcraft_calc::VolatileSource;
use gridcraft_core::{CellRef, MAX_COLS, MAX_ROWS};
use gridcraft_engine::{DocState, RemoteCtx, Session, Trust};
use gridcraft_formula::ids::Lookup;
use gridcraft_model::{Cell, LineInfo, Sheet, StyleId, Workbook};

use crate::axis::AxisKind;
use crate::cells::{Home, from_content};
use crate::doc::{Entry, GridChange, GridDoc};
use crate::ids::{Id, parse_cell_key};
use crate::layout::Layout;
use crate::styles;

/// What a batch is applied against: the base file and the layout over it.
pub(crate) struct Ctx<'a> {
    pub base: &'a Workbook,
    pub layout: &'a Layout,
}

/// The base file's cell at a slot, when both of its lines are the base's.
pub(crate) fn base_cell(base: &Workbook, sheet: usize, row: Id, col: Id) -> Option<&Cell> {
    let at = CellRef::new(row.base_index()?, col.base_index()?);
    base.sheet(sheet)?.cell(at)
}

/// The base file's line information for a line id (default for a line the base lacks).
pub(crate) fn base_line(base: &Workbook, sheet: usize, axis: AxisKind, id: Id) -> LineInfo {
    let (Some(sh), Some(i)) = (base.sheet(sheet), id.base_index()) else { return LineInfo::default() };
    lines_of(sh, axis).get(&i).copied().unwrap_or_default()
}

fn lines_of(sh: &Sheet, axis: AxisKind) -> &std::collections::BTreeMap<u32, LineInfo> {
    if axis == AxisKind::Rows { &sh.rows } else { &sh.cols }
}

/// The line information a sheet holds at a position.
pub(crate) fn line_at(sh: &Sheet, axis: AxisKind, pos: u32) -> LineInfo {
    lines_of(sh, axis).get(&pos).copied().unwrap_or_default()
}

/// Whether a base row is hidden by the base sheet's autofilter: hidden inside its data rows
/// (Ruling R19). Such a row stays hidden whatever the manual flag says.
pub(crate) fn filter_hidden(base: &Workbook, sheet: usize, axis: AxisKind, id: Id) -> bool {
    if axis != AxisKind::Rows {
        return false;
    }
    let (Some(sh), Some(i)) = (base.sheet(sheet), id.base_index()) else { return false };
    let Some(f) = &sh.autofilter else { return false };
    sh.rows.get(&i).is_some_and(|l| l.hidden) && i > f.range.start.row && i <= f.range.end.row
}

/// The part name of a line's size: `h` for a row, `w` for a column.
pub(crate) fn size_part(axis: AxisKind) -> &'static str {
    if axis == AxisKind::Rows { "h" } else { "w" }
}

/// `<id>/<part>`.
pub(crate) fn line_key(id: Id, part: &str) -> String {
    format!("{}/{}", id.text(), part)
}

/// What a batch of remote changes touches.
#[derive(Debug, Default)]
pub(crate) struct Targets {
    cells: BTreeSet<(usize, String)>,
    lines: BTreeSet<(usize, u8, Id)>,
    props: BTreeSet<usize>,
    /// The epoch or seed may have moved: random numbers change.
    pub epoch: bool,
    /// The layout (rows or columns) changed: not supported until G9.
    pub layout: bool,
}

impl Targets {
    /// Whether nothing but possibly the epoch is named.
    pub(crate) fn is_empty(&self) -> bool {
        self.cells.is_empty() && self.lines.is_empty() && self.props.is_empty()
    }

    /// The targets `changes` name. `unresolved` holds style keys a cell named before the style
    /// arrived: when one arrives, the cells that used it are targeted again.
    pub(crate) fn from_changes(layout: &Layout, doc: &GridDoc, changes: &[GridChange], unresolved: &mut BTreeSet<String>) -> Targets {
        let mut t = Targets::default();
        for c in changes {
            match c {
                GridChange::Cells { sheet, keys } | GridChange::Formats { sheet, keys } => {
                    if let Some(i) = layout.sheet_index(*sheet) {
                        t.cells.extend(keys.iter().map(|k| (i, k.clone())));
                    }
                }
                GridChange::Lines { sheet, axis, keys } => {
                    if let Some(i) = layout.sheet_index(*sheet) {
                        t.lines.extend(keys.iter().filter_map(|k| Some((i, u8::from(*axis == AxisKind::Cols), Id::parse(k.split_once('/')?.0)?))));
                    }
                }
                GridChange::SheetProps { sheet } => t.props.extend(layout.sheet_index(*sheet)),
                GridChange::Styles { keys } => t.restyle(layout, doc, keys, unresolved),
                GridChange::Meta => t.epoch = true,
                GridChange::Ops { .. } | GridChange::Axis { .. } => t.layout = true,
            }
        }
        t
    }

    fn restyle(&mut self, layout: &Layout, doc: &GridDoc, keys: &[String], unresolved: &mut BTreeSet<String>) {
        let arrived: Vec<&String> = keys.iter().filter(|k| unresolved.contains(*k)).collect();
        if arrived.is_empty() {
            return;
        }
        for (i, s) in layout.sheets.iter().enumerate() {
            for key in doc.written_cells(s.id) {
                if matches!(doc.fmt(s.id, &key), Entry::Set(k) if arrived.contains(&&k)) {
                    self.cells.insert((i, key));
                }
            }
        }
        for k in arrived.into_iter().cloned().collect::<Vec<_>>() {
            unresolved.remove(&k);
        }
    }

    /// Every key the document holds: the whole overlay.
    pub(crate) fn everything(layout: &Layout, doc: &GridDoc) -> Targets {
        let mut t = Targets { epoch: true, ..Targets::default() };
        for (i, s) in layout.sheets.iter().enumerate() {
            t.cells.extend(doc.written_cells(s.id).into_iter().map(|k| (i, k)));
            for axis in [AxisKind::Rows, AxisKind::Cols] {
                t.lines.extend(doc.written_lines(s.id, axis).iter().filter_map(|k| Some((i, u8::from(axis == AxisKind::Cols), Id::parse(k.split_once('/')?.0)?))));
            }
            t.props.insert(i);
        }
        t
    }
}

/// Applies `t` from `doc`'s current state. Style keys that did not resolve are added to
/// `unresolved`.
pub(crate) fn apply(cx: &mut RemoteCtx, ctx: &Ctx, doc: &GridDoc, t: &Targets, unresolved: &mut BTreeSet<String>) -> gridcraft_engine::Result<()> {
    for (i, key) in &t.cells {
        cell(cx, ctx, doc, *i, key, unresolved)?;
    }
    for (i, cols, id) in &t.lines {
        line(cx, ctx, doc, *i, if *cols == 1 { AxisKind::Cols } else { AxisKind::Rows }, *id)?;
    }
    for i in &t.props {
        props(cx, ctx, doc, *i)?;
    }
    Ok(())
}

fn cell(cx: &mut RemoteCtx, ctx: &Ctx, doc: &GridDoc, i: usize, key: &str, unresolved: &mut BTreeSet<String>) -> gridcraft_engine::Result<()> {
    let (Some((row, col)), Some(sheet)) = (parse_cell_key(key), ctx.layout.sheets.get(i).map(|s| s.id)) else { return Ok(()) };
    // A slot whose row or column is not shown stays in the document, unseen.
    let Some(at) = ctx.layout.cell_at(i, row, col) else { return Ok(()) };
    let home = Home { layout: ctx.layout, sheet: sheet.0 };
    let base = base_cell(ctx.base, i, row, col);
    let mut out = match doc.cell(sheet, key) {
        Entry::Absent => base.filter(|c| c.has_content()).cloned().unwrap_or_default(),
        Entry::Cleared => Cell::default(),
        Entry::Set(c) => from_content(Some(&c), at, &home, StyleId::DEFAULT).unwrap_or_default(),
    };
    out.style = match doc.fmt(sheet, key) {
        Entry::Absent => base.map_or(StyleId::DEFAULT, |b| cx.intern_style(&ctx.base.styles.get(b.style).clone())),
        Entry::Cleared => StyleId::DEFAULT,
        Entry::Set(k) => match doc.style(&k).as_ref().and_then(styles::from_json) {
            Some(s) => cx.intern_style(&s),
            None => {
                log::warn!("gridcraft-live: style {k} is not in the document; the default is shown until it arrives");
                unresolved.insert(k);
                StyleId::DEFAULT
            }
        },
    };
    cx.set_cell(i, at, (!out.is_blank()).then_some(out))
}

fn bool_entry(e: Entry<serde_json::Value>, base: bool) -> bool {
    match e {
        Entry::Absent => base,
        Entry::Cleared => false,
        Entry::Set(v) => v.as_bool().unwrap_or(base),
    }
}

fn line(cx: &mut RemoteCtx, ctx: &Ctx, doc: &GridDoc, i: usize, axis: AxisKind, id: Id) -> gridcraft_engine::Result<()> {
    let Some(sheet) = ctx.layout.sheets.get(i) else { return Ok(()) };
    let Lookup::At(pos) = sheet.axis(axis).pos_of(id) else { return Ok(()) };
    if pos >= if axis == AxisKind::Rows { MAX_ROWS } else { MAX_COLS } {
        return Ok(());
    }
    let cur = cx.workbook().sheet(i).map_or_else(LineInfo::default, |s| line_at(s, axis, pos));
    let base = base_line(ctx.base, i, axis, id);
    let fh = filter_hidden(ctx.base, i, axis, id);
    let size = match doc.line(sheet.id, axis, &line_key(id, size_part(axis))) {
        Entry::Absent => base.size,
        Entry::Cleared => None,
        Entry::Set(v) => v.as_f64().map(|f| f as f32).or(base.size),
    };
    let custom = if axis == AxisKind::Rows { bool_entry(doc.line(sheet.id, axis, &line_key(id, "hc")), base.custom) } else { cur.custom };
    let manual = bool_entry(doc.line(sheet.id, axis, &line_key(id, "hide")), base.hidden && !fh);
    let info = LineInfo { size, hidden: manual || fh, custom, ..cur };
    cx.set_line(i, axis, pos, (info != LineInfo::default()).then_some(info))
}

fn props(cx: &mut RemoteCtx, ctx: &Ctx, doc: &GridDoc, i: usize) -> gridcraft_engine::Result<()> {
    let (Some(sheet), Some(base)) = (ctx.layout.sheets.get(i), ctx.base.sheet(i)) else { return Ok(()) };
    let read = |name: &str, base: f32| doc.sheet_prop(sheet.id, name).and_then(|v| v.as_f64()).map_or(base, |f| f as f32);
    cx.set_default_size(i, AxisKind::Cols, Some(read("defw", base.default_col_width)))?;
    cx.set_default_size(i, AxisKind::Rows, Some(read("defh", base.default_row_height)))
}

/// The workbook the document describes: `base` with the whole overlay applied, laid out and
/// recalculated. Peers apply the same targets incrementally; this applies them at once.
///
/// # Errors
/// [`LiveError::Decode`](craft_collab::LiveError::Decode) when the document names something the
/// engine refuses.
pub(crate) fn build(base: &Workbook, doc: &GridDoc, source: Option<Arc<dyn VolatileSource>>) -> Result<Workbook, craft_collab::LiveError> {
    let fail = |e: gridcraft_engine::EngineError| {
        log::warn!("gridcraft-live: the room's state did not apply: {e}");
        craft_collab::LiveError::Decode
    };
    let layout = Layout::from_doc(doc, base);
    let t = Targets::everything(&layout, doc);
    let mut s = Session::new();
    s.add_document(DocState::new(base.clone(), None, "live".into(), Trust::FromFile));
    let uid = s.active().map(|d| d.uid).ok_or(craft_collab::LiveError::Decode)?;
    s.set_volatile_source(uid, source).map_err(fail)?;
    let ctx = Ctx { base, layout: &layout };
    s.apply_remote(uid, |cx| apply(cx, &ctx, doc, &t, &mut BTreeSet::new())).map_err(fail)?;
    s.active().map(|d| (*d.wb).clone()).ok_or(craft_collab::LiveError::Decode)
}
