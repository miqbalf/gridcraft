//! Ordered edits on the engine and the document: what an op-log entry does to an engine
//! workbook, the steps an author writes into one transaction, and the formulas re-printed from
//! their id form afterwards.

use std::collections::BTreeSet;

use gridcraft_core::{MAX_COLS, MAX_ROWS};
use gridcraft_engine::{DocState, EngineError, RemoteCtx, Session, Trust};
use gridcraft_formula::adjust::Edit;
use gridcraft_formula::ids::{parse_ids, print_ids};
use gridcraft_model::{Cell, Formula, Workbook};

use crate::axis::{AxisKind, Insertion};
use crate::cells::Home;
use crate::doc::{CellContent, Entry, GridDoc, GridTxn};
use crate::ids::{Id, cell_key, parse_cell_key};
use crate::layout::Layout;
use crate::op::Op;
use crate::remote::{Ctx, base_cell};
use crate::reprint::RefIndex;

/// The most lines one ordered edit may name. Its ids travel in one update, with the op that names
/// them: a delete of 40,000 lines measured 1.2 MB there (about 31 bytes a line), over the relay's
/// 1 MiB cap, so the limit leaves a margin under it.
pub const LINES_MAX: usize = 20_000;

/// One thing an author writes into the transaction of an ordered edit, in order.
#[derive(Clone, Debug)]
pub(crate) enum Step {
    Insert { sheet: Id, axis: AxisKind, ins: Insertion, ids: Vec<Id> },
    Delete { sheet: Id, axis: AxisKind, ids: Vec<Id> },
    Undelete { sheet: Id, axis: AxisKind, ids: Vec<Id> },
    Op(Op),
}

/// Base ids an insert far down the sheet writes into one update of their own, at most.
pub(crate) const FILL_MAX: usize = 16_384;

impl Step {
    /// An upper estimate of what the step adds to an update, in bytes: an id in the axis array,
    /// in the op's list, and for a delete in the `gone` map.
    pub(crate) fn cost(&self) -> usize {
        match self {
            Step::Insert { ids, ins, .. } => 64 + ids.len() * 16 + ins.materialise.len() * 12,
            Step::Delete { ids, .. } => 64 + ids.len() * 24,
            Step::Undelete { ids, .. } => 64 + ids.len() * 16,
            Step::Op(op) => match op {
                Op::Insert { ids, .. } | Op::Delete { ids, .. } | Op::Undelete { ids, .. } => 128 + ids.len() * 20,
            },
        }
    }

    pub(crate) fn write(&self, t: &mut GridTxn) {
        match self {
            Step::Insert { sheet, axis, ins, ids } => t.axis_insert(*sheet, *axis, ins, ids),
            Step::Delete { sheet, axis, ids } => t.axis_delete(*sheet, *axis, ids),
            Step::Undelete { sheet, axis, ids } => t.axis_undelete(*sheet, *axis, ids),
            Step::Op(op) => t.push_op(op),
        }
    }
}

/// `before` with the structural `edits` replayed by position, one engine call per edit (never
/// batched: each edit counts in the frame the one before left). `None` when one is refused.
pub(crate) fn replay_before(before: &Workbook, edits: &[(usize, Edit)]) -> Option<Workbook> {
    let mut s = Session::new();
    s.add_document(DocState::new(before.clone(), None, "replay".into(), Trust::FromFile));
    let uid = s.active().map(|d| d.uid)?;
    s.apply_remote(uid, |cx| {
        for (sheet, e) in edits {
            match e {
                Edit::Insert { axis, at, count } => cx.insert_lines(*sheet, *axis, *at, *count)?,
                Edit::Delete { axis, at, count } => {
                    let last = at.checked_add(count.saturating_sub(1)).ok_or_else(|| EngineError::Other("delete out of range".into()))?;
                    cx.delete_lines(*sheet, *axis, &[(*at, last)])?;
                }
                _ => return Err(EngineError::Other("not a line edit".into())),
            }
        }
        Ok(())
    })
    .ok()?;
    s.active().map(|d| (*d.wb).clone())
}

/// What a batch of ops did, for the steps after: which formulas to re-print and which lines came
/// back (their content has to be put in again).
#[derive(Debug, Default)]
pub(crate) struct Applied {
    /// `(sheet index, axis, ids)` of every op, whatever its kind.
    pub touched: Vec<(usize, AxisKind, Vec<Id>)>,
    /// Lines an undelete brought back.
    pub undeleted: Vec<(usize, AxisKind, Vec<Id>)>,
    /// Lines a delete took away, with the positions they had.
    pub deleted: Vec<(usize, AxisKind, Vec<Id>)>,
}

fn limit(axis: AxisKind) -> u32 {
    if axis == AxisKind::Rows { MAX_ROWS } else { MAX_COLS }
}

/// Applies `ops` in order to the engine and to `work`, the layout the engine is at. A sheet an op
/// names that is not there is skipped. Any engine refusal is returned: the caller rebuilds the
/// engine from the document.
pub(crate) fn apply_ops(cx: &mut RemoteCtx, work: &mut Layout, ops: &[Op]) -> gridcraft_engine::Result<Applied> {
    let mut done = Applied::default();
    for op in ops {
        let (sheet, axis, ids) = match op {
            Op::Insert { sheet, axis, ids, .. } | Op::Delete { sheet, axis, ids, .. } | Op::Undelete { sheet, axis, ids, .. } => (*sheet, *axis, ids),
        };
        let Some(i) = work.sheet_index(sheet) else { continue };
        let Some(s) = work.sheets.get_mut(i) else { continue };
        let ax = s.axis_mut(axis);
        let max = limit(axis);
        match op {
            Op::Insert { prev, .. } => {
                let (at, n) = ax.insert_after(*prev, ids);
                let count = u32::try_from(n).unwrap_or(u32::MAX).min(max.saturating_sub(at));
                if count > 0 {
                    cx.insert_lines(i, axis, at, count)?;
                }
            }
            Op::Delete { .. } => {
                let areas: Vec<(u32, u32)> = ax.delete(ids).into_iter().map(|(first, count)| (first, first.saturating_add(count - 1))).collect();
                if !areas.is_empty() {
                    cx.delete_lines(i, axis, &areas)?;
                }
                done.deleted.push((i, axis, ids.clone()));
            }
            Op::Undelete { .. } => {
                for (first, count) in ax.undelete(ids) {
                    cx.insert_lines(i, axis, first, count.min(max.saturating_sub(first)).max(1))?;
                }
                done.undeleted.push((i, axis, ids.clone()));
            }
        }
        done.touched.push((i, axis, ids.clone()));
    }
    Ok(done)
}

/// The id-form text of the formula at `(sheet, key)`: what the document says, else the base
/// file's formula printed over the base layout.
pub(crate) fn formula_source(base: &Workbook, doc: &GridDoc, i: usize, sheet: Id, key: &str) -> Option<String> {
    match doc.cell(sheet, key) {
        Entry::Set(c) => c.f,
        Entry::Cleared => None,
        Entry::Absent => {
            let (r, c) = parse_cell_key(key)?;
            let cell = base_cell(base, i, r, c)?;
            base_source(base, sheet, cell)
        }
    }
}

/// A base formula in id form (the base layout's ids), for the sheet with id `sheet`.
pub(crate) fn base_source(base: &Workbook, sheet: Id, cell: &Cell) -> Option<String> {
    let e = cell.formula.as_ref()?.expr_arc()?;
    print_ids(&e, sheet.0, &Layout::of_base(base)).ok()
}

/// Builds the reference index from the base formulas and the written ones, if it is not built.
pub(crate) fn ensure_index(index: &mut RefIndex, base: &Workbook, layout: &Layout, doc: &GridDoc) {
    if index.built {
        return;
    }
    let of_base = Layout::of_base(base);
    for (i, sheet) in base.sheets.iter().enumerate() {
        let Some(sl) = layout.sheets.get(i) else { continue };
        for (at, cell) in sheet.cells.iter() {
            if cell.formula.is_none() {
                continue;
            }
            let key = cell_key(Id::base(at.row), Id::base(at.col));
            if !matches!(doc.cell(sl.id, &key), Entry::Absent) {
                continue;
            }
            if let Some(e) = cell.formula.as_ref().and_then(|f| f.expr_arc())
                && let Ok(text) = print_ids(&e, sl.id.0, &of_base)
            {
                index.set(sl.id.0, &key, Some(&text));
            }
        }
        for key in doc.written_cells(sl.id) {
            if let Entry::Set(CellContent { f: Some(text), .. }) = doc.cell(sl.id, &key) {
                index.set(sl.id.0, &key, Some(&text));
            }
        }
    }
    index.built = true;
}

/// Re-prints the formulas at `cells` (host sheet id, cell key) from their id-form source over
/// the layout in `ctx`, and sets the ones whose text differs from the engine's (the state wins
/// over the engine's `adjust`; a difference is logged in debug builds).
pub(crate) fn reprint(cx: &mut RemoteCtx, ctx: &Ctx, doc: &GridDoc, cells: &BTreeSet<(u64, String)>) -> gridcraft_engine::Result<()> {
    for (host, key) in cells {
        let Some(i) = ctx.layout.sheet_index(Id(*host)) else { continue };
        let (Some((row, col)), Some(sheet)) = (parse_cell_key(key), ctx.layout.sheets.get(i).map(|s| s.id)) else { continue };
        let Some(at) = ctx.layout.cell_at(i, row, col) else { continue };
        let Some(src) = formula_source(ctx.base, doc, i, sheet, key) else { continue };
        let Some(cur) = cx.workbook().sheet(i).and_then(|s| s.cell(at)).cloned() else { continue };
        let Some(old) = cur.formula.clone() else { continue };
        let Ok(expr) = parse_ids(&src, sheet.0, ctx.layout) else { continue };
        let mut f = Formula::from_expr(expr);
        if f.text == old.text {
            continue;
        }
        if cfg!(debug_assertions) {
            log::warn!("gridcraft-live: the engine's {} differs from the state's {} at {}", old.text, f.text, at.a1());
        }
        f.array = old.array;
        f.dynamic = old.dynamic;
        let cell = Cell { formula: Some(std::sync::Arc::new(f)), ..cur };
        cx.set_cell(i, at, Some(cell))?;
    }
    Ok(())
}

/// The content an author writes for a formula whose id-form text became `text`.
pub(crate) fn rewritten_content(
    base: &Workbook,
    doc: &GridDoc,
    layout: &Layout,
    host: (usize, Id),
    key: &str,
    text: String,
    by: &str,
) -> Option<CellContent> {
    let (i, sheet) = host;
    let home = Home { layout, sheet: sheet.0 };
    let mut c = match doc.cell(sheet, key) {
        Entry::Set(c) => c,
        _ => {
            let (r, col) = parse_cell_key(key)?;
            let cell = base_cell(base, i, r, col)?;
            crate::cells::to_content(cell, &home, by)?
        }
    };
    c.f = Some(text);
    c.u = Some(by.to_owned());
    Some(c)
}

/// Told when an ordered edit names more lines than one update can carry.
pub const TOO_MANY_TEXT: &str = "That many rows or columns cannot be changed at once while editing live. Try fewer.";

/// What an author needs to turn a report's structural edits into steps.
pub(crate) struct Author<'a> {
    pub base: &'a Workbook,
    pub replica: &'a GridDoc,
    pub index: &'a mut RefIndex,
    pub layout: &'a mut Layout,
    pub minted: &'a mut u32,
    pub space: u32,
    pub uid: &'a str,
    pub steps: &'a mut Vec<Step>,
    pub plan: &'a mut crate::split::Plan,
}

impl Author<'_> {
    fn mint(&mut self) -> Id {
        *self.minted = self.minted.saturating_add(1);
        Id::minted(self.space, *self.minted)
    }

    /// The steps of every insert and delete in `r`, in order, and the plan entries that go
    /// with them: the ranges a delete (or an insert next to a deleted line) shrank. What the new
    /// lines took from the line before them is the planner's (`Planner::inserted`).
    pub(crate) fn report(&mut self, r: &gridcraft_engine::EditReport) -> Result<(), craft_live::LocalError> {
        let refuse = || craft_live::LocalError::Refused(crate::gate::NOT_YET_TEXT.to_owned());
        for (i, e) in &r.edits {
            let i = *i;
            let Some(sheet) = self.layout.sheets.get(i).map(|s| s.id) else { return Err(refuse()) };
            match e {
                Edit::Insert { axis, at, count } => {
                    let count = *count as usize;
                    if count > LINES_MAX {
                        return Err(craft_live::LocalError::Refused(TOO_MANY_TEXT.to_owned()));
                    }
                    let ids: Vec<Id> = (0..count).map(|_| self.mint()).collect();
                    // A formula with a range end on a deleted line (typed before its author heard
                    // of the delete) reads as the nearest surviving line. The new lines could
                    // become that line by id, while the engine shifted the range as Excel does:
                    // write the ends the formula shows now, before the insert. Only the deleted
                    // lines right next to where the new lines go can change.
                    ensure_index(self.index, self.base, self.layout, self.replica);
                    let near: Vec<Id> = self
                        .layout
                        .sheets
                        .get(i)
                        .map(|s| {
                            let mut ax = s.axis(*axis).clone();
                            ax.insert(*at, &ids);
                            ax.deleted_next_to(&ids)
                        })
                        .unwrap_or_default();
                    self.shrink(i, sheet, *axis, &near);
                    let Some(s) = self.layout.sheets.get_mut(i) else { return Err(refuse()) };
                    let ins = s.axis_mut(*axis).insert(*at, &ids);
                    let prev = ins.prev;
                    let op_id = self.mint().0;
                    self.steps.push(Step::Insert { sheet, axis: *axis, ins, ids: ids.clone() });
                    self.steps.push(Step::Op(Op::Insert { id: op_id, by: self.uid.to_owned(), sheet, axis: *axis, prev, ids: ids.clone() }));
                }
                Edit::Delete { axis, at, count } => {
                    if *count as usize > LINES_MAX {
                        return Err(craft_live::LocalError::Refused(TOO_MANY_TEXT.to_owned()));
                    }
                    let Some(s) = self.layout.sheets.get(i) else { return Err(refuse()) };
                    let last = at.saturating_add(*count);
                    let ids: Vec<Id> = (*at..last).filter_map(|p| s.axis(*axis).id_at(p)).collect();
                    ensure_index(self.index, self.base, self.layout, self.replica);
                    let Some(s) = self.layout.sheets.get_mut(i) else { return Err(refuse()) };
                    s.axis_mut(*axis).delete(&ids);
                    let op_id = self.mint().0;
                    self.steps.push(Step::Delete { sheet, axis: *axis, ids: ids.clone() });
                    self.steps.push(Step::Op(Op::Delete { id: op_id, by: self.uid.to_owned(), sheet, axis: *axis, ids: ids.clone() }));
                    self.shrink(i, sheet, *axis, &ids);
                }
                _ => return Err(refuse()),
            }
        }
        Ok(())
    }

    /// Writes the formulas that had a range end on a deleted line with that end moved to the
    /// nearest surviving line, so delete-then-insert at the range's new top matches Excel.
    fn shrink(&mut self, i: usize, sheet: Id, axis: AxisKind, ids: &[Id]) {
        for (host, key) in self.index.naming(sheet, axis, ids) {
            let Some(hi) = self.layout.sheet_index(Id(host)) else { continue };
            let Some((row, col)) = parse_cell_key(&key) else { continue };
            // A formula in a deleted line is not shown: leave it as it is.
            if self.layout.cell_at(hi, row, col).is_none() {
                continue;
            }
            let Some(src) = formula_source(self.base, self.replica, hi, Id(host), &key) else { continue };
            let Some(after) = self.layout.sheets.get(i).map(|s| s.axis(axis)) else { continue };
            let Some(new) = crate::reprint::shrink_ends(&src, host, sheet, axis, after) else { continue };
            let Some(content) = rewritten_content(self.base, self.replica, self.layout, (hi, Id(host)), &key, new, self.uid) else { continue };
            self.plan.cell(crate::split::CellWrite {
                sheet: Id(host),
                key,
                content: crate::split::Put::Set(content),
                fmt: crate::split::Put::Keep,
                style: None,
            });
        }
    }
}

/// What an undelete puts back: the lines, and the cells written for them or in the base file.
pub(crate) fn undeleted_targets(
    t: &mut crate::remote::Targets,
    layout: &Layout,
    doc: &GridDoc,
    base: &Workbook,
    i: usize,
    axis: AxisKind,
    ids: &[Id],
) {
    let set: BTreeSet<Id> = ids.iter().copied().collect();
    let Some(sl) = layout.sheets.get(i) else { return };
    for id in ids {
        t.add_line(i, axis, *id);
    }
    for key in doc.written_cells(sl.id) {
        if let Some((r, c)) = parse_cell_key(&key)
            && set.contains(&if axis == AxisKind::Rows { r } else { c })
        {
            t.add_cell(i, key);
        }
    }
    if let Some(sh) = base.sheet(i) {
        for (at, _) in sh.cells.iter() {
            if set.contains(&Id::base(if axis == AxisKind::Rows { at.row } else { at.col })) {
                t.add_cell(i, cell_key(Id::base(at.row), Id::base(at.col)));
            }
        }
    }
}
