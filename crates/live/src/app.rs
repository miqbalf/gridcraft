//! [`GridApp`]: GridCraft's side of each step of a live session, built per call from the engine
//! session and the mapping's state.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use craft_live::{Landed, LiveApp, LocalError};
use gridcraft_core::{CellRef, RangeRef};
use gridcraft_engine::{EngineError, Session};
use gridcraft_model::Workbook;
use serde_json::Value;

use crate::axis::AxisKind;
use crate::doc::{CellContent, Entry, GridChange, GridDoc};
use crate::ids::{Id, parse_cell_key};
use crate::kind::{Grid, GridCaret, GridEdit};
use crate::layout::Layout;
use crate::local::Planner;
use crate::op::Op;
use crate::ops::{Author, Step, apply_ops, ensure_index, formula_source, replay_before, reprint, undeleted_targets};
use crate::remote::{Ctx, Targets, apply, build};
use crate::reprint::RefIndex;
use crate::split::{Plan, Put};
use crate::styles::StyleHash;
use crate::undo::{self, KeyWrite, Replay, Root, Step as UndoStep, UndoLog};
use crate::volatile::LiveVolatile;

/// Told when a change cannot run now because an undo group is open.
pub const BUSY_TEXT: &str = "Finish what you are doing, then try that change again.";
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
    /// This window's own undo and redo steps.
    pub undo: UndoLog,
    /// Ordered undos and redos taken off the stacks, for the session to put in the lock's queue.
    pub replays: Vec<Replay>,
    /// The ordered undo or redo running now: the command it runs lands on the other stack.
    pub replay: Option<Replay>,
    /// Keys each undo left alone, for one notice per undo: (the author's uid if known, where).
    pub undo_skipped: Vec<Vec<(Option<String>, String)>>,
    /// Changes a local call wrote that the engine has yet to take (an undo's writes).
    pub landing: Vec<GridChange>,
    /// An ordered undo or redo is queued for the lock or running: later ones wait behind it.
    pub replay_busy: bool,
    /// Undos (`false`) and redos (`true`) asked for while one waited, in order.
    pub held: Vec<bool>,
    /// How many times the replica was replaced under a running window (a refused run taken back,
    /// the room's checkpoint). The undo log clears on a change of this count.
    pub resets: u32,
    /// The `resets` count the undo state was last cleared for: it clears once per count.
    pub undo_resets: u32,
    /// The base file's name, for the unmerged copy's.
    pub name: String,
    /// Local edits craft-live was about to drop, for [`crate::GridLive::take_unmerged`].
    pub unmerged: Option<crate::unmerged::Unmerged>,
}

impl Mapping {
    /// Keeps `state` (the replica with local edits the room lacks) for the unmerged copy. The
    /// person is told once the copy is built ([`crate::GridLive::take_unmerged`]), never before.
    pub(crate) fn keep_unmerged(&mut self, state: Vec<u8>) {
        self.unmerged.get_or_insert_with(Default::default).states.push(state);
    }

    /// Clears the undo state (steps, queued and running ordered undos, held undos, skipped-key
    /// notes, landed changes) once per `resets` count, and tells the user once if anything was
    /// cleared. The reset hook and the session's frame both call it: the second finds the count
    /// already seen and does nothing. Today only `LiveApp::reset` moves the count in production,
    /// so the frame's calls (`pump`, `after_pump`, `run`) act only for a count moved without the
    /// hook (the test hook, or a future path); they are a guard, not a path to rely on. An ordered
    /// undo already in the lock's queue is out of reach here: it carries its count
    /// ([`Replay::resets`]) and the session drops it when it comes back.
    pub(crate) fn clear_undo_for_resets(&mut self) {
        if self.undo_resets == self.resets {
            return;
        }
        self.undo_resets = self.resets;
        let had = !self.undo.is_empty()
            || !self.replays.is_empty()
            || self.replay.is_some()
            || !self.held.is_empty()
            || self.replay_busy
            || !self.undo_skipped.is_empty()
            || !self.landing.is_empty();
        if had {
            self.notices.push(undo::UNDO_RESET_TEXT.to_owned());
        }
        self.undo = UndoLog::default();
        self.replays.clear();
        self.replay = None;
        self.held.clear();
        self.replay_busy = false;
        self.undo_skipped.clear();
        self.landing.clear();
    }
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
    /// A mapping for a freshly seeded replica: `layout` is revision `layout_rev` of `vol`. Its
    /// `name` (the base file's name, for unmerged copies) starts empty; [`crate::seed::seat`] sets it.
    pub(crate) fn new(doc: u64, base: Arc<Workbook>, layout: Layout, layout_rev: u64, uid: &str, hash: StyleHash, vol: Arc<LiveVolatile>) -> Mapping {
        Mapping {
            doc,
            has_random: crate::local::has_random(&base),
            base,
            layout,
            layout_rev,
            uid: uid.to_owned(),
            space: 0,
            hash,
            keys: HashMap::new(),
            minted: 0,
            vol,
            notices: Vec::new(),
            pending: Vec::new(),
            restore: None,
            recalc: false,
            resync_wanted: false,
            skipped: 0,
            rebuilds: 0,
            unresolved: BTreeSet::new(),
            mine: HashMap::new(),
            lost: Vec::new(),
            editor: None,
            applied_ops: 0,
            index: crate::reprint::RefIndex::default(),
            recent: BTreeMap::new(),
            lines_minted: 0,
            removed: Vec::new(),
            resets: 0,
            undo_resets: 0,
            sent: Vec::new(),
            group: 0,
            undo: crate::undo::UndoLog::default(),
            replays: Vec::new(),
            replay: None,
            undo_skipped: Vec::new(),
            landing: Vec::new(),
            replay_busy: false,
            held: Vec::new(),
            name: String::new(),
            unmerged: None,
        }
    }

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
                    let mut inserted = Vec::new();
                    let replay = if r.edits.is_empty() {
                        None
                    } else {
                        if m.space == 0 {
                            return Err(LocalError::Refused(crate::gate::NOT_YET_TEXT.to_owned()));
                        }
                        ordered = true;
                        let mut wb = replay_before(&r.before, &r.edits).ok_or_else(|| LocalError::Refused(crate::gate::NOT_YET_TEXT.to_owned()))?;
                        let from = steps.len();
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
                        // The new lines took the format of the line before them, in the replay
                        // too, so the planner would see nothing to write: they start blank there
                        // instead, and every part they took (size, set by hand, style) is written.
                        inserted = new_lines(&layout, steps.get(from..).unwrap_or_default());
                        for (i, axis, p) in &inserted {
                            if let Some(sh) = wb.sheet_mut(*i) {
                                let lines = if *axis == AxisKind::Rows { &mut sh.rows } else { &mut sh.cols };
                                lines.remove(p);
                            }
                        }
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
                        inserted: &inserted,
                    };
                    planner.report(r, &mut plan)?;
                    random |= planner.random;
                    resync |= planner.resync;
                }
                GridEdit::Command(_) => {}
                // Taken one by one in `local`, never here.
                GridEdit::Undo | GridEdit::Redo => {}
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
                    let mut naming = m.index.naming(*sheet, *axis, ids);
                    // As for an insert: a range end on a deleted line right next to the lines that
                    // came back can now read (by id) as one of them. Undo gives what the
                    // document says, so those formulas are printed again from it.
                    if let Some(sl) = layout.sheets.get(i) {
                        naming.extend(m.index.naming(*sheet, *axis, &sl.axis(*axis).deleted_next_to(ids)));
                    }
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
        let mut writes: Vec<KeyWrite> = Vec::new();
        for (n, chunk) in plan.split_reserving(reserved).into_iter().enumerate() {
            let keys = chunk_keys(&chunk);
            let befores: Vec<Entry<Value>> = keys.iter().map(|(r, s, k)| undo::read(replica, *r, *s, k)).collect();
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
            for ((root, sheet, key), before) in keys.into_iter().zip(befores) {
                let after = undo::read(replica, root, sheet, &key);
                if after != before {
                    writes.push(KeyWrite { root, sheet, key, before, after });
                }
            }
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
        // One command is one undo step, however many updates it took.
        let replay = m.replay.take();
        if let Some(Replay { step: UndoStep::Ordered { writes: old, .. }, .. }) = &replay
            && !old.is_empty()
        {
            let r = revert(replica, m, group, old)?;
            updates.extend(r.updates);
            writes.extend(r.swapped);
            if !r.skipped.is_empty() {
                m.undo_skipped.push(r.skipped);
            }
            m.landing.extend(undo::changes_of(&r.keys));
        }
        let ops: Vec<Op> = steps.iter().filter_map(|s| if let Step::Op(op) = s { Some(op.clone()) } else { None }).collect();
        let writes = undo::merged(writes);
        let step = if ops.is_empty() {
            (!writes.is_empty()).then_some(UndoStep::Free { writes })
        } else {
            match undo::ordered_step(&ops, &m.uid, writes) {
                Ok(s) => s,
                Err(()) => {
                    // Not one op that can be taken back: nothing before it can be undone either.
                    m.undo = UndoLog::default();
                    m.notices.push(undo::UNDO_CLEARED_TEXT.to_owned());
                    None
                }
            }
        };
        match (step, &replay) {
            (Some(s), Some(r)) => m.undo.record_replay(s, r.redo),
            (Some(s), None) => m.undo.record(s),
            (None, _) => {}
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
                match op {
                    Op::Undelete { .. } => {
                        undeleted_targets(&mut t, &final_layout, replica, &m.base, i, axis, ids);
                        // As for an insert (below): the lines that came back sit next to deleted
                        // lines a range end can be on (G13 fix-1 review N3).
                        if let Some(s) = final_layout.sheets.get(i) {
                            naming.extend(m.index.naming(sheet, axis, &s.axis(axis).deleted_next_to(ids)));
                        }
                    }
                    // The engine gives inserted lines the format of the line before them here,
                    // which need not be the line its author had before them: the document holds
                    // what the author's new lines took, so they are read from it.
                    Op::Insert { .. } => {
                        for id in ids {
                            t.add_line(i, axis, *id);
                        }
                        // A formula with a range end on a deleted line next to the new lines now
                        // reads (by id) as ending on a new line, which the engine's shift does not
                        // know. Its author may not have seen the formula (the relay orders an
                        // edit made under the lock without asking what the holder has seen), so
                        // it is printed again here from the document.
                        if let Some(s) = final_layout.sheets.get(i) {
                            naming.extend(m.index.naming(sheet, axis, &s.axis(axis).deleted_next_to(ids)));
                        }
                    }
                    Op::Delete { .. } => {}
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

impl GridApp<'_> {
    /// A run of edits with no undo or redo among them.
    fn local_run(&mut self, replica: &mut GridDoc, edits: &[GridEdit]) -> Result<Vec<Vec<u8>>, LocalError> {
        let before = self.map.layout.clone();
        let out = self.local_inner(replica, edits);
        if out.is_err() {
            // Nothing of an ordered edit was kept: the layout and the index go back.
            self.map.set_layout(before);
            self.map.index.clear();
            self.map.landing.clear();
            return out;
        }
        let landing = std::mem::take(&mut self.map.landing);
        if !landing.is_empty() {
            self.apply(replica, &landing).map_err(LocalError::Failed)?;
        }
        out
    }

    /// One undo (or redo). A free step is written now, as a new edit of this editor's own keys
    /// only; an ordered one is handed to the session, which runs it through the structure lock.
    fn undo_one(&mut self, replica: &mut GridDoc, redo: bool) -> Result<Vec<Vec<u8>>, LocalError> {
        let m = &mut *self.map;
        if m.replay_busy || !m.replays.is_empty() {
            // An ordered undo waits for the lock: this one goes after it, in order.
            m.held.push(redo);
            return Ok(Vec::new());
        }
        let Some(step) = m.undo.take(redo) else { return Ok(Vec::new()) };
        let writes = match step {
            UndoStep::Free { writes } => writes,
            ordered @ UndoStep::Ordered { .. } => {
                m.replays.push(Replay { redo, step: ordered, resets: m.resets });
                return Ok(Vec::new());
            }
        };
        if m.space == 0 {
            m.undo.put_back(UndoStep::Free { writes }, redo);
            return Err(LocalError::Refused(crate::gate::NOT_YET_TEXT.to_owned()));
        }
        m.group += 1;
        let group = m.group;
        let r = revert(replica, m, group, &writes)?;
        if !r.skipped.is_empty() {
            m.undo_skipped.push(r.skipped);
        }
        if !r.swapped.is_empty() {
            m.undo.record_replay(UndoStep::Free { writes: r.swapped }, redo);
        }
        if !r.updates.is_empty() {
            m.sent.push(SentCommand { group, updates: r.updates.len() });
        }
        let changes = undo::changes_of(&r.keys);
        if !changes.is_empty() {
            self.apply(replica, &changes).map_err(LocalError::Failed)?;
        }
        Ok(r.updates)
    }
}

/// What taking a step's writes back did.
#[derive(Default)]
struct Reverted {
    updates: Vec<Vec<u8>>,
    /// The writes made, as a step the other way.
    swapped: Vec<KeyWrite>,
    /// Keys left alone: (the author's uid if known, where).
    skipped: Vec<(Option<String>, String)>,
    /// Every key written.
    keys: Vec<(Root, Id, String)>,
}

/// Writes each key's `before` back where the key still holds `after`, in updates of `group`
/// within the relay's cap. A cell put back is signed by this editor and names the content it
/// replaced, as any other write does.
fn revert(replica: &mut GridDoc, m: &mut Mapping, group: u64, writes: &[KeyWrite]) -> Result<Reverted, LocalError> {
    let mut out = Reverted::default();
    let mut todo: Vec<(&KeyWrite, Entry<Value>, Entry<Value>)> = Vec::new();
    for w in writes.iter().rev() {
        let now = undo::read(replica, w.root, w.sheet, &w.key);
        if now != w.after {
            let who = match (w.root, &now) {
                (Root::Cells, Entry::Set(v)) => v.get("u").and_then(Value::as_str).map(str::to_owned),
                _ => None,
            };
            let at = place(&m.layout, w.root, w.sheet, &w.key);
            match out.skipped.iter_mut().find(|(_, p)| *p == at) {
                Some(seen) => {
                    if seen.0.is_none() {
                        seen.0 = who;
                    }
                }
                None => out.skipped.push((who, at)),
            }
            continue;
        }
        let target = match (w.root, &w.before) {
            (Root::Cells, Entry::Set(Value::Object(o))) => {
                let seen = match &now {
                    Entry::Set(v) => serde_json::from_value::<CellContent>(v.clone()).map_or_else(|_| "0".to_owned(), |c| c.fingerprint()),
                    _ => "0".to_owned(),
                };
                let mut o = o.clone();
                o.insert("u".into(), Value::String(m.uid.clone()));
                o.insert("p".into(), Value::String(seen));
                Entry::Set(Value::Object(o))
            }
            _ => w.before.clone(),
        };
        todo.push((w, now, target));
    }
    let mut rewrites: Vec<(KeyWrite, Entry<Value>)> = Vec::new();
    let mut start = 0;
    while start < todo.len() {
        let mut used = 0;
        let mut end = start;
        while let Some((w, _, _)) = todo.get(end) {
            let c = undo::cost(w);
            if end > start && used + c > crate::split::CHUNK_BUDGET_BYTES {
                break;
            }
            used += c;
            end += 1;
        }
        let part = todo.get(start..end).unwrap_or_default();
        let update = replica
            .write_grouped(group, |t| {
                for (w, _, target) in part {
                    undo::write(t, w.root, w.sheet, &w.key, target);
                }
            })
            .map_err(|e| LocalError::Failed(e.to_string()))?;
        m.skipped += replica.write_skipped();
        for (w, now, _) in part {
            let after = undo::read(replica, w.root, w.sheet, &w.key);
            if w.root == Root::Cells {
                let k = (w.sheet, w.key.clone());
                match replica.cell(w.sheet, &w.key) {
                    Entry::Set(c) if m.mine.len() < MINE_MAX => {
                        m.recent.insert(k.clone(), c.clone());
                        m.mine.insert(k, c);
                    }
                    _ => {
                        m.mine.remove(&k);
                        m.recent.remove(&k);
                    }
                }
            }
            out.keys.push((w.root, w.sheet, w.key.clone()));
            rewrites.push(((*w).clone(), after.clone()));
            out.swapped.push(KeyWrite { root: w.root, sheet: w.sheet, key: w.key.clone(), before: now.clone(), after });
        }
        out.updates.push(update);
        start = end;
    }
    undo::follow_rewrites(&mut m.undo, &rewrites);
    Ok(out)
}

/// The keys one update writes, where an undo will look for them.
fn chunk_keys(chunk: &crate::split::Chunk) -> Vec<(Root, Id, String)> {
    let mut out = Vec::new();
    for w in &chunk.cells {
        if !matches!(w.content, Put::Keep) {
            out.push((Root::Cells, w.sheet, w.key.clone()));
        }
        if !matches!(w.fmt, Put::Keep) {
            out.push((Root::Fmt, w.sheet, w.key.clone()));
        }
    }
    for ((sheet, axis, key), put) in &chunk.lines {
        if !matches!(put, Put::Keep) {
            out.push((Root::Line(*axis), *sheet, key.clone()));
        }
    }
    for ((sheet, name), _) in &chunk.props {
        out.push((Root::Prop, *sheet, (*name).to_owned()));
    }
    out
}

/// A key's name for the user: `B7` for a cell, `row 7` or `column B` for a line.
fn place(layout: &Layout, root: Root, sheet: Id, key: &str) -> String {
    let i = layout.sheet_index(sheet);
    match root {
        Root::Cells | Root::Fmt => {
            parse_cell_key(key).and_then(|(r, c)| i.and_then(|i| layout.cell_at(i, r, c))).map_or_else(|| key.to_owned(), |c| c.a1())
        }
        Root::Line(axis) => {
            let pos = key.split_once('/').and_then(|(id, _)| Id::parse(id)).and_then(|id| match layout.sheets.get(i?)?.axis(axis).pos_of(id) {
                gridcraft_formula::ids::Lookup::At(p) => Some(p),
                _ => None,
            });
            match (axis, pos) {
                (crate::axis::AxisKind::Rows, Some(p)) => format!("row {}", p + 1),
                (_, Some(p)) => format!("column {}", CellRef::new(0, p).a1().trim_end_matches(|ch: char| ch.is_ascii_digit())),
                _ => "a row or column".to_owned(),
            }
        }
        Root::Prop => match key {
            "defw" => "the default column width".to_owned(),
            "defh" => "the default row height".to_owned(),
            _ => "a sheet setting".to_owned(),
        },
    }
}

impl LiveApp<Grid> for GridApp<'_> {
    fn local(&mut self, replica: &mut GridDoc, edits: &[GridEdit]) -> Result<Vec<Vec<u8>>, LocalError> {
        // Undo and redo are taken one at a time, after the edits before them are written.
        // Once a part is written into the replica it must be sent: when a later part fails, what
        // was written goes out, the rest is told, and the engine is rebuilt from the replica (it
        // ran the refused commands already).
        let mut out = Vec::new();
        let mut rest = edits;
        while !rest.is_empty() {
            let n = rest.iter().position(|e| matches!(e, GridEdit::Undo | GridEdit::Redo)).unwrap_or(rest.len());
            let (run, tail) = rest.split_at(n);
            let (part, next) = match tail.split_first() {
                Some((e, tail)) if run.is_empty() => (self.undo_one(replica, matches!(e, GridEdit::Redo)), tail),
                _ => (self.local_run(replica, run), tail),
            };
            match part {
                Ok(updates) => out.extend(updates),
                Err(e) if out.is_empty() => return Err(e),
                Err(e) => {
                    match e {
                        LocalError::Refused(why) => self.map.notices.push(why),
                        LocalError::Failed(why) => log::warn!("gridcraft-live: part of a batch did not land ({why}); rebuilding"),
                    }
                    self.map.resync_wanted = true;
                    return Ok(out);
                }
            }
            rest = next;
        }
        Ok(out)
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
        // The workbook is rebuilt from the room: a copied or cut range may name other cells now.
        self.session.cancel_copy_mode_for(self.map.doc);
        let held = self.map.layout.clone();
        self.set_layout(Layout::from_doc(replica, &self.map.base));
        let m = &mut *self.map;
        m.index.clear();
        m.recent.clear();
        m.vol.follow(replica);
        let source: Arc<dyn gridcraft_calc::VolatileSource> = m.vol.clone();
        let fresh = match build(&m.base, replica, Some(source)) {
            Ok(wb) => wb,
            Err(e) => {
                self.set_layout(held);
                return Err(e.to_string());
            }
        };
        // A late joiner learns of a random function only through the state it was given.
        self.map.has_random |= crate::local::has_random(&fresh);
        let Some(cur) = self.current() else {
            self.set_layout(held);
            return Err(EngineError::NoDocument.to_string());
        };
        let out = self.install(Arc::new(keep_view(fresh, &cur)));
        if self.map.restore.is_some() {
            // An undo group is open: the engine still shows what it showed, so the layout stays
            // what the engine's is until `retry` installs the rebuilt workbook.
            self.set_layout(held);
        } else {
            self.map.applied_ops = replica.ops_len();
        }
        out
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
        // A reset mid-session (a fenced run taken back, or the room's checkpoint replacing the
        // replica) drops the commands not taken yet: the link has taken their edits back too.
        // `removed` stays: those are other peers' deletes, still true. The undo log clears on a
        // change of `resets`.
        m.sent.clear();
        m.resets = m.resets.saturating_add(1);
        // The undo steps name keys and ops of the replica that was replaced: none can be trusted.
        m.clear_undo_for_resets();
    }

    fn set_space(&mut self, space: u32) {
        self.map.space = space;
        self.map.lines_minted = 0;
    }

    /// craft-live is about to drop local edits the room does not have (a run taken back, a
    /// replica that needs a resync, is damaged, was replaced or was refused): keep them as the
    /// unmerged copy, so the host can save them.
    fn unmerged(&mut self, state: &[u8], edits: usize) {
        log::warn!("gridcraft-live: {edits} local edits the room does not have are kept as an unmerged copy");
        self.map.keep_unmerged(state.to_vec());
    }

    fn caret(&self, payload: &Value) -> Option<GridCaret> {
        let mut c = crate::presence::parse(payload)?;
        crate::presence::resolve_at(&self.map.layout, self.map.layout_rev, &mut c);
        Some(c)
    }

    fn follow(&self, carets: &mut HashMap<String, GridCaret>) {
        // A caret on an id not known yet stays (hidden) and shows when the id arrives.
        for c in carets.values_mut() {
            crate::presence::resolve_at(&self.map.layout, self.map.layout_rev, c);
        }
    }
}

/// Where the lines that `steps` inserted are shown in `layout`: (sheet index, axis, position).
fn new_lines(layout: &Layout, steps: &[Step]) -> Vec<(usize, AxisKind, u32)> {
    let mut out = Vec::new();
    for s in steps {
        let Step::Insert { sheet, axis, ids, .. } = s else { continue };
        let Some(i) = layout.sheet_index(*sheet) else { continue };
        let Some(sl) = layout.sheets.get(i) else { continue };
        for id in ids {
            if let gridcraft_formula::ids::Lookup::At(p) = sl.axis(*axis).pos_of(*id) {
                out.push((i, *axis, p));
            }
        }
    }
    out
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
        // The document holds a formula by ids: print it in A1 form with the layout it was
        // written for (the deleted line is still in it here).
        let print = |f: &str| gridcraft_formula::ids::parse_ids(f, s.0, &m.layout).map_or_else(|_| f.to_owned(), |e| gridcraft_formula::print(&e));
        let value = match (&c.f, &c.v) {
            (Some(f), _) => format!("={}", print(f)),
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
