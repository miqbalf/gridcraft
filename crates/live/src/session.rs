//! [`GridLive`]: a GridCraft session in a live room. It owns the `craft-live` link, the mapping
//! between the engine and the room's document, and the command gate; the host owns the engine
//! [`Session`] and passes it to each call.

use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};

use craft_collab::{Hub, LiveError, Replica};
use craft_live::ordered::{Action, Ordered, Polled};
use craft_live::{Access, LiveApp, LiveEditing, LiveStatus, uid_of};
use gridcraft_calc::VolatileSource;
use gridcraft_core::CellRef;
use gridcraft_engine::{DocState, EditObserver, EditReport, Session, Trust};
use gridcraft_model::Workbook;
use serde_json::Value as Json;
use sha2::{Digest, Sha256};

use crate::app::{GridApp, Mapping, caret_payload};
use crate::axis::AxisKind;
use crate::doc::{GridDoc, GridSeed};
use crate::gate::{Class, NOT_YET_TEXT, class};
use crate::ids::Id;
use crate::kind::{Grid, GridEdit, GridPos};
use crate::layout::Layout;
use crate::local::has_random;
use crate::ordered::{
    BUSY_TEXT, LOCK_LOST_BROKEN_TEXT, LOCK_LOST_TEXT, LOCK_LOST_UNRUN_TEXT, Pending, SCOPE, Tracked, request, resolve, send_actions,
};
use crate::remote::build;
use crate::styles::{StyleHash, fnv1a64};
use crate::volatile::LiveVolatile;

/// Another editor, with the selection as the host draws it.
pub type Peer = craft_live::Peer<GridPos>;

/// The relay's schema string for this build: `gridcraft/1_<build>` (Ruling R1). `<build>` is the
/// compile-time `GRIDCRAFT_LIVE_BUILD` (ASCII letters and digits, at most 40), else the crate
/// version, so a room never mixes engine builds.
#[must_use]
pub fn schema() -> String {
    let build: String = option_env!("GRIDCRAFT_LIVE_BUILD").unwrap_or_default().chars().filter(char::is_ascii_alphanumeric).take(40).collect();
    let build = if build.is_empty() { env!("CARGO_PKG_VERSION").to_owned() } else { build };
    format!("gridcraft/1_{build}")
}

/// Why a session did not start.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LiveFail {
    /// The base bytes did not open as a workbook.
    Open(String),
    /// The base bytes are not the file the room was opened on.
    BaseMismatch,
    Live(LiveError),
}

/// What running a command did.
#[derive(Clone, Debug, PartialEq)]
pub enum Ran {
    Done(Json),
    /// An ordered edit waiting for the structure lock (G9; not produced yet).
    Queued,
}

/// The base file a room is opened on.
#[derive(Clone, Copy, Debug)]
pub struct BaseFile<'a> {
    pub bytes: &'a [u8],
    /// The file's name, for the format (`.xlsx`).
    pub name: &'a str,
    /// The hash the room says the base has; checked before anything is opened.
    pub expected_sha256: Option<[u8; 32]>,
}

/// Hears the engine's reports and queues the ones for this session's document.
struct Tap {
    queue: craft_live::EditQueue<GridEdit>,
    doc: Arc<std::sync::atomic::AtomicU64>,
}

impl EditObserver for Tap {
    fn edited(&mut self, report: EditReport) {
        if report.doc == self.doc.load(std::sync::atomic::Ordering::Relaxed) {
            self.queue.push(GridEdit::Command(Box::new(report)));
        }
    }
}

type SharedClock = Arc<Mutex<Box<dyn FnMut() -> f64 + Send>>>;

/// A GridCraft document edited live.
pub struct GridLive<H: Hub> {
    core: LiveEditing<Tracked<H>, Grid>,
    map: Mapping,
    observed: Arc<std::sync::atomic::AtomicU64>,
    /// uid to the display name last seen in the roster.
    names: HashMap<String, String>,
    /// The engine's undo hook panicked and was dropped.
    undo_notified: bool,
    /// The replica fell out of step for good (told once).
    broken_told: bool,
    /// The structure lock's queue (one ordered edit at a time).
    ordered: Ordered<Pending>,
    /// An ordered edit that has the lock but waits for the engine to catch up.
    deferred: Option<Pending>,
    /// How many join answers the hub has seen, and how many of them were told.
    joins: Arc<AtomicU64>,
    seen_joins: u64,
    /// The clock the lock's wait runs on (shared with the link's own).
    clock: Option<SharedClock>,
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    started: Option<std::time::Instant>,
    /// Frames pumped, the clock where there is none.
    frames: u64,
    /// The unlock for a broken replica was sent.
    broken_sent: bool,
}

const NOT_ACTIVE: &str = "The live workbook is not open any more";
/// Told when the replica cannot catch up and the file has to be reopened.
pub const RESYNC_TEXT: &str = "This window fell out of step with the other editors. Reopen the file to keep editing.";
/// Told when the engine's undo hook failed.
pub const UNDO_FAILED_TEXT: &str = "Undo stopped working in this window. Reopen the file to get it back.";

impl<H: Hub> GridLive<H> {
    /// Opens `base` as the session's document, checks its hash when one is given, installs the
    /// edit observer, the live undo hook and the shared random numbers, and seeds the replica.
    ///
    /// `hub` must have joined the room naming [`schema()`] (`NativeHub::connect_as(.., Some(&schema()))`
    /// or the web hub's equivalent), so the relay types the room's top-level roots; [`schema()`] is
    /// the one place the string is made. Give the link a clock with [`GridLive::set_clock`] on the
    /// web, so a lock held for a long run has its lease refreshed.
    ///
    /// # Errors
    /// [`LiveFail`].
    pub fn start(hub: H, room: &str, base: &BaseFile, access: Access, uid: &str, session: &mut Session) -> Result<Self, LiveFail> {
        Self::start_inner(hub, room, base, access, uid, session, fnv1a64)
    }

    /// [`GridLive::start`] with the style key hash given (tests force a collision).
    ///
    /// # Errors
    /// [`LiveFail`].
    #[cfg(any(test, feature = "test-util"))]
    #[doc(hidden)]
    pub fn start_with(
        hub: H,
        room: &str,
        base: &BaseFile,
        access: Access,
        uid: &str,
        session: &mut Session,
        hash: StyleHash,
    ) -> Result<Self, LiveFail> {
        Self::start_inner(hub, room, base, access, uid, session, hash)
    }

    fn start_inner(hub: H, room: &str, base: &BaseFile, access: Access, uid: &str, session: &mut Session, hash: StyleHash) -> Result<Self, LiveFail> {
        let sha: [u8; 32] = Sha256::digest(base.bytes).into();
        if base.expected_sha256.is_some_and(|want| want != sha) {
            return Err(LiveFail::BaseMismatch);
        }
        let (wb, warnings) = gridcraft_engine::io::open_bytes(base.name, base.bytes).map_err(|e| LiveFail::Open(e.to_string()))?;
        if !warnings.is_empty() {
            log::info!("gridcraft-live: opening the base gave {} warnings", warnings.len());
        }
        let seed = GridSeed { base_sha256: sha, sheets: u32::try_from(wb.sheets.len()).unwrap_or(u32::MAX) };
        let (hub, joins) = Tracked::new(hub);
        let core = LiveEditing::<Tracked<H>, Grid>::start(hub, room, &seed, access).map_err(LiveFail::Live)?;
        let title = std::path::Path::new(base.name).file_name().map_or_else(|| base.name.to_owned(), |n| n.to_string_lossy().into_owned());
        session.add_document(DocState::new(wb, None, title, Trust::FromFile));
        let (doc, wb) = session.active().map(|d| (d.uid, d.wb.clone())).ok_or(LiveFail::Open("no workbook".into()))?;
        let replica = GridDoc::seeded(&seed).map_err(LiveFail::Live)?;
        let vol = LiveVolatile::new(&replica, &wb);
        let layout = Layout::from_doc(&replica, &wb);
        let layout_rev = vol.update_layout(layout.clone());
        let map = Mapping {
            doc,
            has_random: has_random(&wb),
            base: wb,
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
            recent: std::collections::BTreeMap::new(),
            lines_minted: 0,
            removed: Vec::new(),
            sent: Vec::new(),
            group: 0,
            undo: crate::undo::UndoLog::default(),
            replays: Vec::new(),
            replay: None,
            undo_skipped: Vec::new(),
            landing: Vec::new(),
            replay_busy: false,
            held: Vec::new(),
        };
        let observed = Arc::new(std::sync::atomic::AtomicU64::new(doc));
        let mut live = GridLive {
            core,
            map,
            observed,
            names: HashMap::new(),
            undo_notified: false,
            broken_told: false,
            ordered: Ordered::new(SCOPE),
            deferred: None,
            joins,
            seen_joins: 0,
            clock: None,
            started: None,
            frames: 0,
            broken_sent: false,
        };
        if let Err(e) = live.bind(session) {
            // Do not leave the document open in the host's session.
            if let Some(i) = session.document_index(doc) {
                session.close_document(i);
            }
            return Err(LiveFail::Open(e.to_string()));
        }
        Ok(live)
    }

    /// Hooks the engine up to the session's document: the edit observer, the live undo hook and
    /// the shared random numbers. Call again when the document is re-created under a new id (a
    /// reopen), after [`GridLive::document_replaced`].
    fn bind(&mut self, session: &mut Session) -> gridcraft_engine::Result<()> {
        session.set_observer(Some(Box::new(Tap { queue: self.core.queue(), doc: self.observed.clone() })));
        self.bind_undo(session)?;
        let source: Arc<dyn VolatileSource> = self.map.vol.clone();
        session.set_volatile_source(self.map.doc, Some(source))
    }

    /// The live undo hook, bound to this session's document by id (not to whichever is active).
    fn bind_undo(&self, session: &mut Session) -> gridcraft_engine::Result<()> {
        let queue = self.core.queue();
        session.set_live_undo_hook_for(
            self.map.doc,
            Some(Box::new(move |redo| {
                queue.push(if redo { GridEdit::Redo } else { GridEdit::Undo });
                true
            })),
        )
    }

    /// The engine document of this session was re-created under `doc` (the host reopened it):
    /// follow it, and set the observer, the undo hook and the random source up again.
    ///
    /// # Errors
    /// The engine refused the new document.
    pub fn document_replaced(&mut self, session: &mut Session, doc: u64) -> Result<(), String> {
        let Some(i) = session.document_index(doc) else { return Err(NOT_ACTIVE.to_owned()) };
        session.set_active(i);
        self.map.doc = doc;
        self.observed.store(doc, std::sync::atomic::Ordering::Relaxed);
        self.bind(session).map_err(|e| e.to_string())
    }

    fn activate(&self, session: &mut Session) -> Result<(), String> {
        let i = session.document_index(self.map.doc).ok_or_else(|| NOT_ACTIVE.to_owned())?;
        if session.active_index() != i {
            session.set_active(i);
        }
        Ok(())
    }

    /// One frame: retries what an open undo group held back, shares the local edits, takes in
    /// the other editors' edits and carets, and says who else is here.
    pub fn pump(&mut self, session: &mut Session) -> Vec<Peer> {
        if self.core.live().unconfirmed_edits() == 0 {
            // Everything this window wrote has been ordered: no delete can have missed it.
            self.map.recent.clear();
        }
        let peers = {
            let mut app = GridApp { session, map: &mut self.map };
            self.core.pump(&mut app)
        };
        // After the flush: a refused edit is put back first, so changes held back for an open
        // undo group are applied on top of that and not wiped by it.
        let retry = {
            let mut app = GridApp { session, map: &mut self.map };
            app.retry(self.core.live().doc())
        };
        if let Err(e) = retry {
            log::warn!("gridcraft-live: a held-back change did not land: {e}");
            self.core.end(RESYNC_TEXT);
        }
        for p in &peers {
            self.names.insert(uid_of(&p.id).to_owned(), p.display.clone());
        }
        self.drive_ordered(session);
        self.after_pump(session);
        peers
    }

    fn after_pump(&mut self, session: &Session) {
        if self.core.live().doc().is_some_and(GridDoc::needs_resync) && !self.broken_told {
            // craft-live gives no way to replace a replica in place, and a full state must never
            // be applied onto the stuck one: end the link, and the reopened file builds a new one.
            self.broken_told = true;
            self.core.end(RESYNC_TEXT);
        }
        if session.live_undo_failed() && !self.undo_notified {
            self.undo_notified = true;
            self.map.notices.push(UNDO_FAILED_TEXT.to_owned());
        }
        for (uid, cell) in std::mem::take(&mut self.map.lost) {
            let who = self.names.get(&uid).cloned().unwrap_or(uid);
            self.map.notices.push(format!("{who} also changed {cell}; their value was kept."));
        }
        for (uid, cell, value, what) in std::mem::take(&mut self.map.removed) {
            let who = self.names.get(&uid).cloned().unwrap_or(uid);
            self.map.notices.push(format!("{who} deleted the {what} where you had typed {value}. Your entry in {cell} was removed."));
        }
        for skipped in std::mem::take(&mut self.map.undo_skipped) {
            let named: Vec<(Option<String>, String)> =
                skipped.into_iter().map(|(uid, at)| (uid.map(|u| self.names.get(&u).cloned().unwrap_or(u)), at)).collect();
            self.map.notices.push(crate::undo::skipped_text(&named));
        }
        if self.map.undo.take_trimmed() {
            self.map.notices.push(crate::undo::UNDO_TRIMMED_TEXT.to_owned());
        }
        self.start_replays();
        for n in std::mem::take(&mut self.map.notices) {
            self.core.push_notice(n);
        }
    }

    /// Puts the ordered undos and redos the last flush took off the stacks into the lock's queue,
    /// unless someone else already built on their lines.
    fn start_replays(&mut self) {
        for r in std::mem::take(&mut self.map.replays) {
            if let Some(text) = self.replay_refusal(&r) {
                self.map.notices.push(text);
                self.replay_over();
                continue;
            }
            let crate::undo::Step::Ordered { inverse, region: (sheet, axis, ids), .. } = &r.step else { continue };
            let pending = match &**inverse {
                crate::op::Op::Undelete { .. } => Pending {
                    command: String::new(),
                    anchors: Vec::new(),
                    count: 0,
                    undelete: Some((*sheet, *axis, ids.clone())),
                    undo: Some(r.clone()),
                },
                _ => Pending {
                    command: if *axis == AxisKind::Rows { "home.deleteRows" } else { "home.deleteColumns" }.to_owned(),
                    anchors: ids.iter().map(|line| crate::ordered::Slot { sheet: *sheet, axis: *axis, line: *line }).collect(),
                    count: 0,
                    undelete: None,
                    undo: Some(r.clone()),
                },
            };
            match self.enqueue(pending) {
                Ok(_) => self.map.replay_busy = true,
                Err(text) => {
                    self.map.undo.put_back(r.step, r.redo);
                    self.map.notices.push(text);
                    self.replay_over();
                }
            }
        }
    }

    /// The ordered undo or redo is over (run, refused or given up): the undos and redos asked
    /// for meanwhile are queued again, in order, for the next flush.
    fn replay_over(&mut self) {
        self.map.replay_busy = false;
        for redo in std::mem::take(&mut self.map.held) {
            self.core.queue().push(if redo { GridEdit::Redo } else { GridEdit::Undo });
        }
    }

    /// An ordered undo or redo that never ran (nothing was written) goes back on its stack.
    fn replay_unrun(&mut self, p: &Pending) {
        if let Some(r) = &p.undo {
            self.map.undo.put_back(r.step.clone(), r.redo);
            self.replay_over();
        }
    }

    /// "Alice changed these rows since; not undone" when a later op by someone else touched the
    /// lines an ordered undo would change.
    fn replay_refusal(&self, r: &crate::undo::Replay) -> Option<String> {
        let crate::undo::Step::Ordered { op, region, .. } = &r.step else { return None };
        let log = self.core.live().doc().map(|d| d.ops_from(0)).unwrap_or_default();
        let what = if region.1 == AxisKind::Rows { "rows" } else { "columns" };
        match crate::undo::touched_by(&log, op, region, &self.map.uid) {
            crate::undo::Touched::No => None,
            crate::undo::Touched::By(uid) => {
                let who = self.names.get(&uid).cloned().unwrap_or(uid);
                Some(format!("{who} changed these {what} since; not undone"))
            }
            crate::undo::Touched::Unknown => Some(format!("These {what} may have been changed since; not undone")),
        }
    }

    /// The gate: LOCAL and FREE commands run now; an ORDERED one waits for the lock (G9: refused
    /// for now); anything else is refused with [`NOT_YET_TEXT`], and the workbook is untouched.
    ///
    /// # Errors
    /// The refusal, or the command's own error, as text for the user.
    pub fn run(&mut self, session: &mut Session, id: &str, params: Json) -> Result<Ran, String> {
        match class(id) {
            Class::Local => {
                self.activate(session)?;
                let done = session.execute(id, params).map_err(|e| e.to_string())?;
                self.flush(session);
                Ok(Ran::Done(done))
            }
            Class::Free => {
                self.activate(session)?;
                if !self.core.editable() {
                    return Err(self.core.not_editable());
                }
                let done = session.execute(id, params).map_err(|e| e.to_string())?;
                if id == "formulas.calculateNow" {
                    self.core.queue().push(GridEdit::CalculateNow);
                }
                self.flush(session);
                Ok(Ran::Done(done))
            }
            Class::Ordered => {
                self.activate(session)?;
                if !self.core.editable() {
                    return Err(self.core.not_editable());
                }
                let pending = request(session, &self.map.layout, id, &params)?;
                self.enqueue(pending)
            }
            Class::NotYet => Err(NOT_YET_TEXT.to_owned()),
        }
    }

    /// Brings deleted lines back (the undo of a delete, G11's, and tests): an `Undelete` op,
    /// through the same lock as any ordered edit.
    ///
    /// # Errors
    /// The refusal, as text for the user.
    pub fn undelete_lines(&mut self, session: &mut Session, sheet: Id, axis: AxisKind, ids: Vec<Id>) -> Result<Ran, String> {
        self.activate(session)?;
        if !self.core.editable() {
            return Err(self.core.not_editable());
        }
        self.enqueue(Pending { command: String::new(), anchors: Vec::new(), count: 0, undelete: Some((sheet, axis, ids)), undo: None })
    }

    fn enqueue(&mut self, pending: Pending) -> Result<Ran, String> {
        let now = self.now();
        match self.ordered.request(pending, now) {
            Ok(actions) => {
                self.send(&actions);
                Ok(Ran::Queued)
            }
            Err(_) => Err(BUSY_TEXT.to_owned()),
        }
    }

    fn now(&mut self) -> f64 {
        if let Some(c) = &self.clock {
            return c.lock().map_or(0.0, |mut f| f());
        }
        self.native_ms().unwrap_or(self.frames as f64 * 16.0)
    }

    /// Milliseconds since the first call; none on the web, which has no clock (the host gives one
    /// with [`GridLive::set_clock`], and frames stand in for it).
    #[cfg(not(target_arch = "wasm32"))]
    fn native_ms(&mut self) -> Option<f64> {
        #[allow(clippy::disallowed_methods)] // off wasm only, as clippy.toml asks
        let start = *self.started.get_or_insert_with(std::time::Instant::now);
        Some(start.elapsed().as_secs_f64() * 1000.0)
    }

    #[cfg(target_arch = "wasm32")]
    fn native_ms(&mut self) -> Option<f64> {
        None
    }

    fn send(&mut self, actions: &[Action]) {
        send_actions(&mut self.core, actions);
    }

    /// One frame of the ordered edit's queue: the relay's word on the lock, a rejoin, the edit
    /// itself when it may run, and the unlock once the edit is ordered.
    fn drive_ordered(&mut self, session: &mut Session) {
        self.frames = self.frames.saturating_add(1);
        let now = self.now();
        if matches!(self.core.status(), LiveStatus::Broken(_)) && !self.broken_sent {
            self.broken_sent = true;
            let actions = self.ordered.replica_broken();
            self.send(&actions);
        }
        for ev in self.core.take_lock_events() {
            let actions = self.ordered.on_lock(&ev, now);
            self.send(&actions);
        }
        let joins = self.joins.load(std::sync::atomic::Ordering::Relaxed);
        if joins > self.seen_joins {
            if self.seen_joins > 0 {
                let actions = self.ordered.rejoined(now);
                self.send(&actions);
            }
            self.seen_joins = joins;
        }
        // The queue's own state stays the judge, deferred edit or not: a rejoin while the edit
        // waits for an open undo group ends as `LockLost`, never as a run without the lock.
        let polled = match self.ordered.poll(self.core.newest_seq(), now) {
            Polled::Nothing => self.deferred.take().map_or(Polled::Nothing, Polled::Run),
            other => other,
        };
        match polled {
            Polled::Nothing => {}
            Polled::Run(p) => self.run_pending(session, p),
            Polled::GaveUp { holder, item } => {
                self.replay_unrun(&item);
                let who = self.holder_name(&holder);
                self.map.notices.push(format!("{who} is still changing the sheet's structure. Your change was not made; try again in a moment."));
            }
            Polled::LockLost { .. } => {
                if let Some(p) = self.deferred.take() {
                    // It never ran: nothing was changed or shared.
                    self.replay_unrun(&p);
                    self.map.notices.push(LOCK_LOST_UNRUN_TEXT.to_owned());
                } else if matches!(self.core.status(), LiveStatus::Broken(_)) {
                    // The replica broke: the relay never answered for the edit.
                    self.map.notices.push(LOCK_LOST_BROKEN_TEXT.to_owned());
                } else {
                    // The relay refused the edit (another editor held the lock by then). The link
                    // has taken it out of the replica and `LiveApp::reset` and `resync` have put
                    // the engine back at the room's state: only the user needs telling.
                    self.map.notices.push(LOCK_LOST_TEXT.to_owned());
                }
            }
        }
        if self.deferred.is_none() {
            let settled = self.core.local_settled();
            let actions = self.ordered.finish(settled);
            self.send(&actions);
        }
    }

    fn holder_name(&self, holder: &str) -> String {
        let uid = uid_of(holder);
        self.names
            .get(uid)
            .cloned()
            .filter(|_| !holder.is_empty())
            .unwrap_or_else(|| if holder.is_empty() { "The relay".to_owned() } else { "Another editor".to_owned() })
    }

    /// Runs an ordered edit that holds the lock: its lines are found again by id, the command runs
    /// on today's positions, and its edit is shared in one transaction.
    fn run_pending(&mut self, session: &mut Session, p: Pending) {
        if !self.core.editable() {
            let why = self.core.not_editable();
            self.map.notices.push(why);
            self.replay_unrun(&p);
            return;
        }
        if !self.map.pending.is_empty() || self.map.restore.is_some() {
            // The engine has not applied everything the room holds (an undo group is open).
            self.deferred = Some(p);
            return;
        }
        if let Err(e) = self.activate(session) {
            self.map.notices.push(e);
            self.replay_unrun(&p);
            return;
        }
        if let Some(r) = &p.undo {
            // Checked again now the lock is held: an op may have come in while it waited.
            if let Some(text) = self.replay_refusal(r) {
                self.map.notices.push(text);
                self.replay_over();
                return;
            }
            // Anything else queued goes out first, so it is not taken into the undo's step.
            self.flush(session);
        }
        // The command an undo runs lands on the other stack and does not clear redo.
        self.map.replay = p.undo.clone();
        if let Some((sheet, axis, ids)) = p.undelete {
            self.core.queue().push(GridEdit::Undelete { sheet, axis, ids });
            self.flush(session);
        } else {
            match resolve(&p, &self.map.layout) {
                Err(text) => self.map.notices.push(text),
                Ok(params) => match session.execute(&p.command, params) {
                    Ok(_) => self.flush(session),
                    Err(e) => self.map.notices.push(e.to_string()),
                },
            }
        }
        self.map.replay = None;
        if p.undo.is_some() {
            self.replay_over();
        }
    }

    fn flush(&mut self, session: &mut Session) {
        let mut app = GridApp { session, map: &mut self.map };
        self.core.flush(&mut app);
        self.after_pump(session);
    }

    /// Whether the host may offer `id` now: LOCAL and FREE commands (ordered ones from G9).
    #[must_use]
    pub fn allows(&self, id: &str) -> bool {
        matches!(class(id), Class::Local | Class::Free | Class::Ordered)
    }

    #[must_use]
    pub fn status(&self) -> &LiveStatus {
        self.core.status()
    }

    /// "Waiting for Bob's change…" while an ordered edit waits for another editor's lock.
    #[must_use]
    pub fn status_line(&self) -> Option<String> {
        let holder = self.ordered.waiting_for()?;
        Some(format!("Waiting for {}'s change…", self.holder_name(holder)))
    }

    /// Builds the engine from a checkpoint's state, as the link does when the room's snapshot
    /// replaces the replica: nothing is replayed, and the applied-ops count is the log's length.
    /// For tests.
    ///
    /// # Errors
    /// The state did not decode, or did not build.
    #[doc(hidden)]
    pub fn apply_checkpoint(&mut self, session: &mut Session, state: &[u8]) -> Result<(), String> {
        let doc = GridDoc::from_state(state).map_err(|e| e.to_string())?;
        self.activate(session)?;
        let space = self.map.space;
        let mut app = GridApp { session, map: &mut self.map };
        LiveApp::reset(&mut app, space);
        LiveApp::resync(&mut app, &doc)
    }

    /// How many times an ordered change made the engine rebuild from the document instead of
    /// applying the ops (for tests: it should stay 0).
    #[doc(hidden)]
    #[must_use]
    pub fn ordered_rebuilds(&self) -> u32 {
        self.map.rebuilds
    }

    /// How many op-log entries the engine has applied (equal to the log's length when it is
    /// caught up, or was built from the state).
    #[must_use]
    pub fn applied_ops(&self) -> u32 {
        self.map.applied_ops
    }

    pub fn take_notices(&mut self) -> Vec<String> {
        for n in std::mem::take(&mut self.map.notices) {
            self.core.push_notice(n);
        }
        // The link words a refused run in terms of its scope; the lock-lost notice above says it
        // for a user, so the two do not both reach the screen.
        let mut notices = self.core.take_notices();
        if notices.iter().any(|n| n == LOCK_LOST_TEXT) {
            notices.retain(|n| !n.starts_with("another editor took "));
        }
        notices
    }

    /// Whether the host may take an edit now.
    #[must_use]
    pub fn editable(&self) -> bool {
        self.core.editable()
    }

    /// Tells the room where this window's selection is (held to five a second by the link).
    pub fn send_selection(&mut self, session: &Session) {
        let Some(d) = session.documents().iter().find(|d| d.uid == self.map.doc) else { return };
        let editing = self.map.editor.is_some();
        let sel = &d.selection;
        if let Some(p) = caret_payload(&self.map.layout, d.wb.active_sheet(), sel.active, &sel.ranges, editing) {
            self.core.send_presence(p);
        }
    }

    /// Marks the cell the editor is open on (or none), by slot, so it stays on its content when
    /// rows or columns move.
    pub fn anchor_editor(&mut self, at: Option<(usize, CellRef)>) {
        self.map.editor = at.and_then(|(sheet, cell)| {
            let (r, c) = self.map.layout.slot_at(sheet, cell)?;
            Some((self.map.layout.sheets.get(sheet)?.id, r, c))
        });
    }

    /// Where the cell the editor is open on is now, or `None` when it is gone.
    #[must_use]
    pub fn editor_at(&self, _session: &Session) -> Option<(usize, CellRef)> {
        let (sheet, r, c) = self.map.editor?;
        let i = self.map.layout.sheet_index(sheet)?;
        Some((i, self.map.layout.cell_at(i, r, c)?))
    }

    pub fn set_clock(&mut self, clock: Box<dyn FnMut() -> f64 + Send>) {
        let shared = Arc::new(Mutex::new(clock));
        let link = shared.clone();
        self.core.set_clock(Box::new(move || link.lock().map_or(0.0, |mut f| f())));
        self.clock = Some(shared);
    }

    #[must_use]
    pub fn core(&self) -> &LiveEditing<Tracked<H>, Grid> {
        &self.core
    }

    pub fn core_mut(&mut self) -> &mut LiveEditing<Tracked<H>, Grid> {
        &mut self.core
    }

    /// The commands sent since the last call: each is one group (one logical step for undo) even
    /// when it went out as several updates.
    pub fn take_commands(&mut self) -> Vec<crate::app::SentCommand> {
        std::mem::take(&mut self.map.sent)
    }

    /// The replica, for tests and the worker.
    #[must_use]
    pub fn replica(&self) -> Option<&GridDoc> {
        self.core.live().doc()
    }

    /// Ends the session: the engine's observer, undo hook and random source come off, so the
    /// workbook is an ordinary one again.
    pub fn stop(self, session: &mut Session) {
        session.set_observer(None);
        if session.live_undo_doc() == Some(self.map.doc) {
            // `set_live_undo_hook_for` refuses a closed document; this clears the hook either way.
            session.set_live_undo_hook(None);
        }
        if session.document_index(self.map.doc).is_some() {
            // Clearing a source cannot fail for a document that is open.
            let _ = session.set_volatile_source(self.map.doc, None);
        }
    }
}

/// The workbook the worker saves and the tests compare against: `base` with the replica state
/// applied, laid out and recalculated. `source` supplies the random numbers (see
/// [`volatile_for`]); `None` uses this process's own.
///
/// # Errors
/// [`LiveError`] when the state does not decode or does not apply.
pub fn workbook_from_state(base: &Workbook, state: &[u8], source: Option<Arc<dyn VolatileSource>>) -> Result<Workbook, LiveError> {
    let doc = GridDoc::from_state(state)?;
    build(base, &doc, source)
}

/// The shared random source for the room `state` holds, over `base`.
///
/// # Errors
/// [`LiveError`] when the state does not decode.
pub fn volatile_for(base: &Workbook, state: &[u8]) -> Result<Arc<dyn VolatileSource>, LiveError> {
    let doc = GridDoc::from_state(state)?;
    let source: Arc<dyn VolatileSource> = LiveVolatile::new(&doc, base);
    Ok(source)
}
