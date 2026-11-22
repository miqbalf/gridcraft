//! [`GridApp`]: GridCraft's side of each step of a live session, built per call from the engine
//! session and the mapping's state.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use craft_live::{Landed, LiveApp, LocalError};
use gridcraft_core::{CellRef, RangeRef};
use gridcraft_engine::{EngineError, Session};
use gridcraft_model::Workbook;
use serde_json::{Value, json};

use crate::doc::{CellContent, Entry, GridChange, GridDoc};
use crate::ids::{Id, parse_cell_key};
use crate::kind::{Grid, GridCaret, GridEdit, GridPos};
use crate::layout::Layout;
use crate::local::Planner;
use crate::op::Op;
use crate::ops::{Author, Step, apply_ops, ensure_index, formula_source, replay_before, reprint, undeleted_targets};
use crate::remote::{Ctx, Targets, apply, build};
use crate::reprint::RefIndex;
use crate::split::{Plan, Put};
use crate::styles::StyleHash;
use crate::volatile::LiveVolatile;

/// Told when Undo or Redo is asked for: per-user undo comes with G11.
pub const UNDO_NOT_YET_TEXT: &str = "Undo and Redo are not available while editing live yet";
/// Told when a change cannot run now because an undo group is open.
pub const BUSY_TEXT: &str = "Finish what you are doing, then try that change again.";
/// The most ranges of a caret kept.
const CARET_RANGES_MAX: usize = 64;
/// The most of this peer's own writes remembered for "your value was replaced" notices.
const MINE_MAX: usize = 200_000;

/// What the mapping keeps between calls.
pub(crate) struct Mapping {
    /// The engine document's uid.
    pub doc: u64,
    pub base: Arc<Workbook>,
    /// Never assign this directly: use [`GridApp::set_layout`].
    pub layout: Layout,
    /// The revision of `layout` inside `vol`, set by `set_layout`.
    pub layout_rev: u64,
    pub uid: String,
    pub space: u32,
    pub hash: StyleHash,
    /// Canonical style JSON to the key it was written under.
    pub keys: HashMap<String, String>,
    pub minted: u32,
    pub vol: Arc<LiveVolatile>,
    /// The workbook holds a formula that calls a random function.
    pub has_random: bool,
    /// Lines for the user, taken into the session's notices at the next pump.
    pub notices: Vec<String>,
    /// Remote changes that came while an undo group was open.
    pub pending: Vec<GridChange>,
    /// A workbook to put back that could not be while an undo group was open.
    pub restore: Option<Arc<Workbook>>,
    /// Every formula is to be recalculated (the epoch moved) and could not be yet.
    pub recalc: bool,
    /// The local engine differs from the room (a hidden flag): rebuild it after the batch.
    pub resync_wanted: bool,
    /// Writes the last batch skipped, for `landed`.
    pub skipped: usize,
    /// Times an ordered change fell back to a rebuild from the document.
    pub rebuilds: u32,
    /// Style keys a cell named before the style arrived.
    pub unresolved: BTreeSet<String>,
    /// This peer's own cell content writes, to tell it when another's replaced one.
    pub mine: HashMap<(Id, String), CellContent>,
    /// Another peer's change replaced one of this peer's: (their uid, the cell).
    pub lost: Vec<(String, String)>,
    /// The commands sent so far, as the group they were written under and how many updates each
    /// took (a large batch is cut into several updates that are still one command).
    pub sent: Vec<SentCommand>,
    /// The last command group handed out.
    pub group: u64,
    /// The slot the cell editor is open on: (sheet, row, column).
    pub editor: Option<(Id, Id, Id)>,
    /// How many op-log entries the engine has applied (or is built from).
    pub applied_ops: u32,
    /// Which formula cells name each line (built when the first ordered edit needs it).
    pub index: RefIndex,
    /// This peer's cell writes the room may not have seen yet, to tell it when a delete took
    /// the row or column it typed into.
    pub recent: BTreeMap<(Id, String), CellContent>,
    /// Line ids this peer minted in its space.
    pub lines_minted: u32,
    /// Entries a delete took away: (their uid, the cell, the value as text, `row` or `column`).
    pub removed: Vec<(String, String, String, &'static str)>,
}

/// One command as it went out: its group (shared by all its updates, see
/// [`GridDoc::write_grouped`]) and how many updates it took.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SentCommand {
    pub group: u64,
    pub updates: usize,
}

/// The engine session and the mapping, for one call.
pub(crate) struct GridApp<'a> {
    pub session: &'a mut Session,
    pub map: &'a mut Mapping,
}

/// `fresh` with this window's own view state (zoom, scroll, gridlines, active sheet) from `cur`:
/// the view is never shared, so rebuilding the shared part must not reset it.
pub(crate) fn keep_view(mut fresh: Workbook, cur: &Workbook) -> Workbook {
    if fresh.sheets.len() == cur.sheets.len() {
        for (f, c) in fresh.sheets.iter_mut().zip(&cur.sheets) {
            let f = Arc::make_mut(f);
            f.show_gridlines = c.show_gridlines;
            f.show_headings = c.show_headings;
            f.show_formulas = c.show_formulas;
            f.show_zeros = c.show_zeros;
            f.show_outline_symbols = c.show_outline_symbols;
            f.zoom = c.zoom;
            f.view_active = c.view_active;
            f.view_top_left = c.view_top_left;
            f.view = c.view;
        }
        fresh.active_sheet = cur.active_sheet.clone();
    }
    fresh.view = cur.view;
    fresh.show_objects = cur.show_objects;
    fresh
}

impl Mapping {
    /// The one place the layout changes: the mapping's copy and the random source's copy move
    /// together. G9 calls this (through [`GridApp::set_layout`]) after every ordered op, local or
    /// remote, before anything recalculates.
    pub(crate) fn set_layout(&mut self, layout: Layout) {
        self.layout_rev = self.vol.update_layout(layout.clone());
        self.layout = layout;
    }

    /// Before a recalculation: when the random source holds another layout than this mapping
    /// (a change that did not go through `set_layout`), give it ours and say so.
    pub(crate) fn sync_layout(&mut self) {
        let held = self.vol.layout_rev();
        if held != self.layout_rev {
            log::warn!("gridcraft-live: the random source held layout revision {held}, not {}; replaced", self.layout_rev);
            self.layout_rev = self.vol.update_layout(self.layout.clone());
        }
    }
}

impl GridApp<'_> {
    /// Takes a new layout after an ordered op (see [`Mapping::set_layout`]).
    pub(crate) fn set_layout(&mut self, layout: Layout) {
        self.map.set_layout(layout);
    }

    fn current(&self) -> Option<Arc<Workbook>> {
        self.session.documents().iter().find(|d| d.uid == self.map.doc).map(|d| d.wb.clone())
    }

    /// Puts `wb` in place quietly; held back for a retry while an undo group is open.
    fn install(&mut self, wb: Arc<Workbook>) -> Result<(), String> {
        match self.session.replace_workbook_quiet(self.map.doc, wb.clone()) {
            Ok(()) => {
                self.map.restore = None;
                Ok(())
            }
            Err(EngineError::RemoteBusy) => {
                self.map.restore = Some(wb);
                Ok(())
            }
            Err(e) => Err(e.to_string()),
        }
    }

    /// Recalculates every formula (the epoch moved).
    fn recalc_all(&mut self) {
        self.map.sync_layout();
        let Some(wb) = self.current() else { return };
        match self.session.replace_workbook_quiet(self.map.doc, wb) {
            Ok(()) => self.map.recalc = false,
            Err(EngineError::RemoteBusy) => self.map.recalc = true,
            Err(e) => log::warn!("gridcraft-live: recalculation after an epoch change failed: {e}"),
        }
    }

    /// Retries what an open undo group held back; call at the start of each pump.
    pub(crate) fn retry(&mut self, replica: Option<&GridDoc>) -> Result<(), String> {
        if let Some(wb) = self.map.restore.take() {
            // Remote changes may have landed since `wb` was taken: rebuild from the replica
            // (which holds them) and not from the stale copy.
            match replica {
                Some(doc) => self.resync(doc)?,
                None => self.install(wb)?,
            }
        }
        if !self.map.pending.is_empty()
            && let Some(doc) = replica
        {
            self.apply(doc, &[])?;
        }
        if self.map.recalc && self.map.pending.is_empty() && self.map.restore.is_none() {
            self.recalc_all();
        }
        Ok(())
    }

    fn local_inner(&mut self, replica: &mut GridDoc, edits: &[GridEdit]) -> Result<Vec<Vec<u8>>, LocalError> {
        let GridApp { session, map: m } = self;
        let mut plan = Plan::default();
        let mut steps: Vec<Step> = Vec::new();
        let mut layout = m.layout.clone();
        let mut minted = m.lines_minted;
        let (mut calculate, mut random, mut resync, mut ordered) = (false, false, false, false);
        for e in edits {
            match e {
                GridEdit::Command(r) if r.doc == m.doc => {
                    let replay = if r.edits.is_empty() {
                        None
                    } else {
                        if m.space == 0 {
                            return Err(LocalError::Refused(crate::gate::NOT_YET_TEXT.to_owned()));
                        }
                        ordered = true;
                        let wb = replay_before(&r.before, &r.edits).ok_or_else(|| LocalError::Refused(crate::gate::NOT_YET_TEXT.to_owned()))?;
                        let mut author = Author {
                            base: &m.base,
                            replica,
                            index: &mut m.index,
                            layout: &mut layout,
                            minted: &mut minted,
                            space: m.space,
                            uid: &m.uid,
                            steps: &mut steps,
                            plan: &mut plan,
                        };
                        author.report(r)?;
                        Some(wb)
                    };
                    let mut planner = Planner {
                        base: &m.base,
                        layout: &layout,
                        doc: replica,
                        uid: &m.uid,
                        space: m.space,
                        hash: m.hash,
                        keys: &mut m.keys,
                        minted: &mut m.minted,
                        resync: false,
                        random: false,
                        before: replay.as_ref(),
                    };
                    planner.report(r, &mut plan)?;
                    random |= planner.random;
                    resync |= planner.resync;
                }
                GridEdit::Command(_) => {}
                GridEdit::Undo | GridEdit::Redo => m.notices.push(UNDO_NOT_YET_TEXT.to_owned()),
                GridEdit::CalculateNow => calculate = true,
                GridEdit::Undelete { sheet, axis, ids } => {
                    let Some(i) = layout.sheet_index(*sheet) else { continue };
                    if m.space == 0 || ids.len() > crate::ops::LINES_MAX {
                        return Err(LocalError::Refused(crate::ops::TOO_MANY_TEXT.to_owned()));
                    }
                    ordered = true;
                    let Some(sl) = layout.sheets.get_mut(i) else { continue };
                    let areas = sl.axis_mut(*axis).undelete(ids);
                    minted = minted.saturating_add(1);
                    let op = Op::Undelete { id: Id::minted(m.space, minted).0, by: m.uid.clone(), sheet: *sheet, axis: *axis, ids: ids.clone() };
                    steps.push(Step::Undelete { sheet: *sheet, axis: *axis, ids: ids.clone() });
                    steps.push(Step::Op(op));
                    ensure_index(&mut m.index, &m.base, &layout, replica);
                    let naming = m.index.naming(*sheet, *axis, ids);
                    let mut t = Targets::default();
                    undeleted_targets(&mut t, &layout, replica, &m.base, i, *axis, ids);
                    m.set_layout(layout.clone());
                    let ctx = Ctx { base: &m.base, layout: &layout };
                    let unresolved = &mut m.unresolved;
                    let doc: &GridDoc = replica;
                    let done = session.apply_remote(m.doc, |cx| {
                        for (first, count) in &areas {
                            cx.insert_lines(i, *axis, *first, *count)?;
                        }
                        apply(cx, &ctx, doc, &t, unresolved)?;
                        reprint(cx, &ctx, doc, &naming)
                    });
                    match done {
                        Ok(()) => {}
                        Err(EngineError::RemoteBusy) => return Err(LocalError::Refused(BUSY_TEXT.to_owned())),
                        Err(e) => return Err(LocalError::Failed(e.to_string())),
                    }
                }
            }
        }
        m.has_random |= random;
        m.resync_wanted |= resync;
        plan.epoch = calculate || (m.has_random && !plan.is_empty());
        if plan.is_empty() && steps.is_empty() {
            return Ok(Vec::new());
        }
        let epoch = plan.epoch;
        let mut updates = Vec::new();
        m.group += 1;
        let group = m.group;
        // Base ids an insert far down the sheet must write first travel in updates of their own:
        // they change nothing a reader sees, so the edit itself stays small.
        for s in &mut steps {
            if let Step::Insert { sheet, axis, ins, .. } = s
                && ins.materialise.len() > crate::ops::FILL_MAX
            {
                let all = std::mem::take(&mut ins.materialise);
                for piece in all.chunks(crate::ops::FILL_MAX) {
                    let fill = crate::axis::Insertion { index: 0, materialise: piece.to_vec(), prev: None };
                    let update =
                        replica.write_grouped(group, |t| t.axis_insert(*sheet, *axis, &fill, &[])).map_err(|e| LocalError::Failed(e.to_string()))?;
                    updates.push(update);
                }
            }
        }
        let reserved: usize = steps.iter().map(Step::cost).sum();
        for (n, chunk) in plan.split_reserving(reserved).into_iter().enumerate() {
            let update = replica
                .write_grouped(group, |t| {
                    if n == 0 {
                        for s in &steps {
                            s.write(t);
                        }
                    }
                    chunk.write(t, &m.uid);
                })
                .map_err(|e| LocalError::Failed(e.to_string()))?;
            m.skipped += replica.write_skipped();
            for w in &chunk.cells {
                match &w.content {
                    crate::split::Put::Set(_) if m.mine.len() < MINE_MAX => {
                        if let Entry::Set(stored) = replica.cell(w.sheet, &w.key) {
                            m.recent.insert((w.sheet, w.key.clone()), stored.clone());
                            m.mine.insert((w.sheet, w.key.clone()), stored);
                        }
                    }
                    Put::Keep => {}
                    _ => {
                        m.mine.remove(&(w.sheet, w.key.clone()));
                        m.recent.remove(&(w.sheet, w.key.clone()));
                    }
                }
                if m.index.built
                    && !matches!(w.content, Put::Keep)
                    && let Some(i) = layout.sheet_index(w.sheet)
                {
                    let text = formula_source(&m.base, replica, i, w.sheet, &w.key);
                    m.index.set(w.sheet.0, &w.key, text.as_deref());
                }
            }
            updates.push(update);
        }
        m.sent.push(SentCommand { group, updates: updates.len() });
        if ordered {
            m.set_layout(layout);
            m.lines_minted = minted;
            m.applied_ops = replica.ops_len();
            if m.has_random {
                m.vol.follow(replica);
                m.recalc = true;
            }
        }
        if epoch {
            m.vol.follow(replica);
            m.recalc = true;
        }
        Ok(updates)
    }

    /// Keeps the reference index in step with the cells the changes name.
    fn index_changes(&mut self, replica: &GridDoc, changes: &[GridChange]) {
        let m = &mut *self.map;
        if !m.index.built {
            return;
        }
        for c in changes {
            let GridChange::Cells { sheet, keys } = c else { continue };
            let Some(i) = m.layout.sheet_index(*sheet) else { continue };
            for k in keys {
                let text = formula_source(&m.base, replica, i, *sheet, k);
                m.index.set(sheet.0, k, text.as_deref());
            }
        }
    }

    /// Applies new op-log entries and the state changes of the same updates: the ops in array
    /// order through the engine (the layout moving with each), then the state over the result,
    /// then the formulas that name the ops' lines re-printed from their id form. When the engine
    /// refuses an op, or lands on another layout than the document's, it is rebuilt from the
    /// document instead.
    fn apply_ordered(&mut self, replica: &GridDoc, all: Vec<GridChange>, t: Targets) -> Result<(), String> {
        let GridApp { session, map: m } = self;
        let mut t = t;
        let final_layout = Layout::from_doc(replica, &m.base);
        let ops = replica.ops_from(m.applied_ops);
        let new_len = replica.ops_len();
        ensure_index(&mut m.index, &m.base, &final_layout, replica);
        for c in &all {
            let GridChange::Cells { sheet, keys } = c else { continue };
            let Some(i) = m.layout.sheet_index(*sheet) else { continue };
            for k in keys {
                let text = formula_source(&m.base, replica, i, *sheet, k);
                m.index.set(sheet.0, k, text.as_deref());
            }
        }
        let mut naming = BTreeSet::new();
        for op in &ops {
            let (sheet, axis, ids) = match op {
                Op::Insert { sheet, axis, ids, .. } | Op::Delete { sheet, axis, ids, .. } | Op::Undelete { sheet, axis, ids, .. } => {
                    (*sheet, *axis, ids)
                }
            };
            naming.extend(m.index.naming(sheet, axis, ids));
            if let Some(i) = final_layout.sheet_index(sheet) {
                if matches!(op, Op::Undelete { .. }) {
                    undeleted_targets(&mut t, &final_layout, replica, &m.base, i, axis, ids);
                }
                if let Op::Delete { by, .. } = op
                    && by != &m.uid
                {
                    removed_notices(m, sheet, axis, ids, by);
                }
            }
        }
        let sel = session.documents().iter().find(|d| d.uid == m.doc).map(|d| (d.wb.active_sheet(), d.selection.clone()));
        let old_layout = m.layout.clone();
        m.set_layout(final_layout.clone());
        if t.epoch {
            m.vol.follow(replica);
        }
        let mut work = old_layout.clone();
        let ctx = Ctx { base: &m.base, layout: &final_layout };
        let unresolved = &mut m.unresolved;
        let res = session.apply_remote(m.doc, |cx| {
            apply_ops(cx, &mut work, &ops)?;
            if !work.equivalent(&final_layout) {
                return Err(EngineError::Other("the op log did not reach the document's layout".into()));
            }
            apply(cx, &ctx, replica, &t, unresolved)?;
            reprint(cx, &ctx, replica, &naming)
        });
        match res {
            Ok(()) => {
                m.applied_ops = new_len;
                if let Some((sheet, sel)) = sel {
                    follow_selection(session, m.doc, sheet, &sel, &old_layout, &m.layout);
                }
                if m.has_random {
                    self.recalc_all();
                }
                Ok(())
            }
            Err(EngineError::RemoteBusy) => {
                m.set_layout(old_layout);
                m.pending = all;
                Ok(())
            }
            Err(e) => {
                log::warn!("gridcraft-live: an ordered edit did not land ({e}); rebuilding from the document");
                m.removed.clear();
                m.rebuilds = m.rebuilds.saturating_add(1);
                self.resync(replica)
            }
        }
    }

    fn a1(&self, sheet: Id, key: &str) -> String {
        let at = parse_cell_key(key).and_then(|(r, c)| self.map.layout.sheet_index(sheet).and_then(|i| self.map.layout.cell_at(i, r, c)));
        at.map_or_else(|| key.to_owned(), |c| c.a1())
    }
}

impl LiveApp<Grid> for GridApp<'_> {
    fn local(&mut self, replica: &mut GridDoc, edits: &[GridEdit]) -> Result<Vec<Vec<u8>>, LocalError> {
        let before = self.map.layout.clone();
        let out = self.local_inner(replica, edits);
        if out.is_err() {
            // Nothing of an ordered edit was kept: the layout and the index go back.
            self.map.set_layout(before);
            self.map.index.clear();
        }
        out
    }

    fn landed(&mut self) -> Landed {
        let out = if std::mem::take(&mut self.map.skipped) > 0 {
            Landed::Partial
        } else if std::mem::take(&mut self.map.resync_wanted) {
            Landed::Resync
        } else {
            Landed::Done
        };
        if self.map.recalc {
            self.recalc_all();
        }
        out
    }

    fn put_back(&mut self, edits: Vec<GridEdit>) -> Result<(), String> {
        let Some(before) = edits.iter().find_map(|e| if let GridEdit::Command(r) = e { Some(r.before.clone()) } else { None }) else { return Ok(()) };
        let Some(cur) = self.current() else { return Ok(()) };
        let wb = keep_view((*before).clone(), &cur);
        self.install(Arc::new(wb))
    }

    fn received(&mut self, replica: &mut GridDoc, changes: Vec<GridChange>) -> (Vec<GridChange>, Vec<Vec<u8>>) {
        for c in &changes {
            let GridChange::Cells { sheet, keys } = c else { continue };
            for k in keys {
                let Entry::Set(now) = replica.cell(*sheet, k) else {
                    self.map.mine.remove(&(*sheet, k.clone()));
                    continue;
                };
                if now.f.as_deref().is_some_and(crate::cells::is_random) {
                    self.map.has_random = true;
                }
                let key = (*sheet, k.clone());
                if let Some(mine) = self.map.mine.get(&key)
                    && *mine != now
                {
                    // The overwrite is concurrent only when its author had not seen our value.
                    let concurrent = now.p.as_ref().is_some_and(|p| *p != mine.fingerprint());
                    self.map.mine.remove(&key);
                    if concurrent && let Some(who) = now.u.clone().filter(|u| *u != self.map.uid) {
                        let cell = self.a1(*sheet, k);
                        self.map.lost.push((who, cell));
                    }
                }
            }
        }
        (changes, Vec::new())
    }

    fn apply(&mut self, replica: &GridDoc, changes: &[GridChange]) -> Result<(), String> {
        let mut all = std::mem::take(&mut self.map.pending);
        all.extend_from_slice(changes);
        let m = &mut *self.map;
        if all.is_empty() && replica.ops_len() <= m.applied_ops {
            return Ok(());
        }
        let t = Targets::from_changes(&m.layout, replica, &all, &mut m.unresolved);
        if t.layout || replica.ops_len() > m.applied_ops {
            return self.apply_ordered(replica, all, t);
        }
        if t.epoch {
            m.vol.follow(replica);
        }
        m.sync_layout();
        if !t.is_empty() {
            let ctx = Ctx { base: &m.base, layout: &m.layout };
            let unresolved = &mut m.unresolved;
            match self.session.apply_remote(m.doc, |cx| apply(cx, &ctx, replica, &t, unresolved)) {
                Ok(()) => {}
                Err(EngineError::RemoteBusy) => {
                    m.pending = all;
                    return Ok(());
                }
                Err(e) => return Err(e.to_string()),
            }
        }
        self.index_changes(replica, &all);
        if t.epoch && self.map.has_random {
            self.recalc_all();
        }
        Ok(())
    }

    fn resync(&mut self, replica: &GridDoc) -> Result<(), String> {
        self.map.pending.clear();
        let layout = Layout::from_doc(replica, &self.map.base);
        self.set_layout(layout);
        let m = &mut *self.map;
        m.applied_ops = replica.ops_len();
        m.index.clear();
        m.recent.clear();
        m.vol.follow(replica);
        let source: Arc<dyn gridcraft_calc::VolatileSource> = m.vol.clone();
        let fresh = build(&m.base, replica, Some(source)).map_err(|e| e.to_string())?;
        // A late joiner learns of a random function only through the state it was given.
        m.has_random |= crate::local::has_random(&fresh);
        let cur = self.current().ok_or_else(|| EngineError::NoDocument.to_string())?;
        self.install(Arc::new(keep_view(fresh, &cur)))
    }

    fn reset(&mut self, space: u32) {
        let m = &mut *self.map;
        m.space = space;
        m.keys.clear();
        m.mine.clear();
        m.recent.clear();
        m.pending.clear();
        m.unresolved.clear();
        m.index.clear();
    }

    fn set_space(&mut self, space: u32) {
        self.map.space = space;
        self.map.lines_minted = 0;
    }

    fn caret(&self, payload: &Value) -> Option<GridCaret> {
        let sheet = Id::parse(payload.get("s")?.as_str()?)?;
        let active = parse_cell_key(payload.get("a")?.as_str()?)?;
        let ranges: Vec<((Id, Id), (Id, Id))> = payload
            .get("r")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .take(CARET_RANGES_MAX)
                    .filter_map(|v| {
                        let (x, y) = v.as_str()?.split_once(':')?;
                        Some((parse_cell_key(x)?, parse_cell_key(y)?))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let editing = payload.get("e").and_then(Value::as_bool).unwrap_or(false);
        let mut c = GridCaret { sheet, active, ranges, editing, pos: GridPos { sheet: 0, active: CellRef::default(), ranges: Vec::new(), editing } };
        resolve(&self.map.layout, &mut c).then_some(c)
    }

    fn follow(&self, carets: &mut HashMap<String, GridCaret>) {
        carets.retain(|_, c| resolve(&self.map.layout, c));
    }
}

/// Works out where a caret is now; false when its sheet or active cell is gone.
fn resolve(layout: &Layout, c: &mut GridCaret) -> bool {
    let Some(i) = layout.sheet_index(c.sheet) else { return false };
    let Some(active) = layout.cell_at(i, c.active.0, c.active.1) else { return false };
    let ranges =
        c.ranges.iter().filter_map(|((r0, c0), (r1, c1))| Some(RangeRef::new(layout.cell_at(i, *r0, *c0)?, layout.cell_at(i, *r1, *c1)?))).collect();
    c.pos = GridPos { sheet: i, active, ranges, editing: c.editing };
    true
}

/// The caret payload for a selection: ids, so it follows its cells through inserts and deletes.
pub(crate) fn caret_payload(layout: &Layout, sheet: usize, active: CellRef, ranges: &[RangeRef], editing: bool) -> Option<Value> {
    let key = |at: CellRef| layout.slot_at(sheet, at).map(|(r, c)| crate::ids::cell_key(r, c));
    let rs: Vec<String> = ranges.iter().take(CARET_RANGES_MAX).filter_map(|r| Some(format!("{}:{}", key(r.start)?, key(r.end)?))).collect();
    Some(json!({"s": layout.sheets.get(sheet)?.id.text(), "a": key(active)?, "r": rs, "e": editing}))
}

/// Notes the entries this peer typed that a delete by `by` took away (it had not seen them).
fn removed_notices(m: &mut Mapping, sheet: Id, axis: crate::axis::AxisKind, ids: &[Id], by: &str) {
    let set: BTreeSet<Id> = ids.iter().copied().collect();
    let what = if axis == crate::axis::AxisKind::Rows { "row" } else { "column" };
    let hits: Vec<(Id, String, CellContent)> = m
        .recent
        .iter()
        .filter(|((s, key), _)| {
            *s == sheet && parse_cell_key(key).is_some_and(|(r, c)| set.contains(&if axis == crate::axis::AxisKind::Rows { r } else { c }))
        })
        .map(|((s, key), c)| (*s, key.clone(), c.clone()))
        .collect();
    for (s, key, c) in hits {
        let at = parse_cell_key(&key)
            .and_then(|(r, col)| m.layout.sheet_index(s).and_then(|i| m.layout.cell_at(i, r, col)))
            .map_or_else(|| key.clone(), |cell| cell.a1());
        let value = match (&c.f, &c.v) {
            (Some(f), _) => format!("={f}"),
            (None, Some(Value::String(t))) => t.clone(),
            (None, Some(v)) => v.to_string(),
            _ => String::new(),
        };
        m.removed.push((by.to_owned(), at, value, what));
        m.recent.remove(&(s, key));
    }
}

/// Moves the selection with its cells: each corner is looked up by id in the new layout (a
/// deleted line gives its neighbour's position).
fn follow_selection(session: &mut Session, doc: u64, sheet: usize, sel: &gridcraft_engine::Selection, old: &Layout, new: &Layout) {
    let move_cell = |c: CellRef| -> CellRef {
        let Some((r, col)) = old.slot_at(sheet, c) else { return c };
        let Some(s) = new.sheets.get(sheet) else { return c };
        let pos = |ax: &crate::axis::Axis, id: Id, was: u32| match ax.pos_of(id) {
            gridcraft_formula::ids::Lookup::At(p) => p,
            gridcraft_formula::ids::Lookup::Deleted { after, before } => after.or(before).unwrap_or(was),
            gridcraft_formula::ids::Lookup::Unknown => was,
        };
        CellRef::new(pos(&s.rows, r, c.row), pos(&s.cols, col, c.col))
    };
    let moved = gridcraft_engine::Selection {
        active: move_cell(sel.active),
        anchor: move_cell(sel.anchor),
        ranges: sel.ranges.iter().map(|r| RangeRef::new(move_cell(r.start), move_cell(r.end))).collect(),
    };
    if moved.active != sel.active || moved.anchor != sel.anchor || moved.ranges != sel.ranges {
        // The document is open (the apply just ran on it).
        let _ = session.set_selection_quiet(doc, moved);
    }
}
