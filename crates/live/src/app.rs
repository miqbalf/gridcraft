//! [`GridApp`]: GridCraft's side of each step of a live session, built per call from the engine
//! session and the mapping's state.

use std::collections::{BTreeSet, HashMap};
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
use crate::remote::{Ctx, Targets, apply, build};
use crate::split::{Plan, Put};
use crate::styles::StyleHash;
use crate::volatile::LiveVolatile;

/// Told when Undo or Redo is asked for: per-user undo comes with G11.
pub const UNDO_NOT_YET_TEXT: &str = "Undo and Redo are not available while editing live yet";
/// Told when another peer ran an ordered edit this build cannot apply.
pub const ORDERED_REMOTE_TEXT: &str =
    "Another editor changed rows or columns, which this version cannot follow yet. Reopen the file to keep editing.";
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

    fn a1(&self, sheet: Id, key: &str) -> String {
        let at = parse_cell_key(key).and_then(|(r, c)| self.map.layout.sheet_index(sheet).and_then(|i| self.map.layout.cell_at(i, r, c)));
        at.map_or_else(|| key.to_owned(), |c| c.a1())
    }
}

impl LiveApp<Grid> for GridApp<'_> {
    fn local(&mut self, replica: &mut GridDoc, edits: &[GridEdit]) -> Result<Vec<Vec<u8>>, LocalError> {
        let mut plan = Plan::default();
        let m = &mut *self.map;
        let mut planner = Planner {
            base: &m.base,
            layout: &m.layout,
            doc: replica,
            uid: &m.uid,
            space: m.space,
            hash: m.hash,
            keys: &mut m.keys,
            minted: &mut m.minted,
            resync: false,
            random: false,
        };
        let mut calculate = false;
        for e in edits {
            match e {
                GridEdit::Command(r) if r.doc == m.doc => planner.report(r, &mut plan)?,
                GridEdit::Command(_) => {}
                GridEdit::Undo | GridEdit::Redo => m.notices.push(UNDO_NOT_YET_TEXT.to_owned()),
                GridEdit::CalculateNow => calculate = true,
            }
        }
        let (random, resync) = (planner.random, planner.resync);
        m.has_random |= random;
        m.resync_wanted |= resync;
        plan.epoch = calculate || (m.has_random && !plan.is_empty());
        if plan.is_empty() {
            return Ok(Vec::new());
        }
        let epoch = plan.epoch;
        let mut updates = Vec::new();
        m.group += 1;
        let group = m.group;
        for chunk in plan.split() {
            let update = replica.write_grouped(group, |t| chunk.write(t, &m.uid)).map_err(|e| LocalError::Failed(e.to_string()))?;
            m.skipped += replica.write_skipped();
            for w in &chunk.cells {
                match &w.content {
                    crate::split::Put::Set(_) if m.mine.len() < MINE_MAX => {
                        if let Entry::Set(stored) = replica.cell(w.sheet, &w.key) {
                            m.mine.insert((w.sheet, w.key.clone()), stored);
                        }
                    }
                    Put::Keep => {}
                    _ => {
                        m.mine.remove(&(w.sheet, w.key.clone()));
                    }
                }
            }
            updates.push(update);
        }
        m.sent.push(SentCommand { group, updates: updates.len() });
        if epoch {
            m.vol.follow(replica);
            m.recalc = true;
        }
        Ok(updates)
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
        if all.is_empty() {
            return Ok(());
        }
        let m = &mut *self.map;
        let t = Targets::from_changes(&m.layout, replica, &all, &mut m.unresolved);
        if t.layout {
            m.notices.push(ORDERED_REMOTE_TEXT.to_owned());
            return Err(ORDERED_REMOTE_TEXT.to_owned());
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
        if t.epoch && m.has_random {
            self.recalc_all();
        }
        Ok(())
    }

    fn resync(&mut self, replica: &GridDoc) -> Result<(), String> {
        self.map.pending.clear();
        let layout = Layout::from_doc(replica, &self.map.base);
        self.set_layout(layout);
        let m = &mut *self.map;
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
        m.pending.clear();
        m.unresolved.clear();
    }

    fn set_space(&mut self, space: u32) {
        self.map.space = space;
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
