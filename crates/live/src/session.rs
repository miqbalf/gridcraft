//! [`GridLive`]: a GridCraft session in a live room. It owns the `craft-live` link, the mapping
//! between the engine and the room's document, and the command gate; the host owns the engine
//! [`Session`] and passes it to each call.

use std::collections::HashMap;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};

use craft_collab::{ClientMsg, Hub, LiveError, Replica};
use craft_live::ordered::{Action, Ordered, Polled};
use craft_live::{Access, LiveApp, LiveEditing, LiveStatus, uid_of};
use gridcraft_calc::VolatileSource;
use gridcraft_core::CellRef;
use gridcraft_engine::{EditObserver, EditReport, Session};
use gridcraft_model::Workbook;
use serde_json::Value as Json;

use crate::app::{GridApp, Mapping};
use crate::axis::AxisKind;
use crate::doc::GridDoc;
use crate::gate::{Class, NOT_YET_TEXT, class};
use crate::ids::Id;
use crate::kind::{Grid, GridEdit, GridPos};
use crate::ordered::{
    BUSY_TEXT, LOCK_LOST_BROKEN_TEXT, LOCK_LOST_KEPT_TEXT, LOCK_LOST_LINK_TEXT, LOCK_LOST_TEXT, LOCK_LOST_UNRUN_LINK_TEXT, LOCK_LOST_UNRUN_TEXT,
    Pending, SCOPE, TAKEN_BACK_TEXT, Tracked, request, resolve, send_actions,
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

impl std::fmt::Display for LiveFail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LiveFail::Open(e) => write!(f, "the base file did not open: {e}"),
            LiveFail::BaseMismatch => f.write_str("the base is not the file the room was opened on"),
            LiveFail::Live(e) => write!(f, "the live session did not start: {e}"),
        }
    }
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
    /// Whether the link rejoined since the current run started.
    rejoined_in_run: bool,
    /// `map.resets` when the current run started, and when the notices last looked.
    run_resets: u32,
    /// The run was already dropped and told (the rejoin before it ran): nothing more to say.
    run_told: bool,
    /// A lost lock waiting to be told once the run has settled: `resets` when the run began, and
    /// whether the link rejoined during it.
    lost_pending: Option<(u32, bool)>,
    /// The clock the lock's wait runs on (shared with the link's own).
    clock: Option<SharedClock>,
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    started: Option<std::time::Instant>,
    /// Frames pumped, the clock where there is none.
    frames: u64,
    /// The unlock for a broken replica was sent.
    broken_sent: bool,
    /// The room this session joined, to leave it by name.
    room: String,
    /// The person's local date and time, for the unmerged copy's name (the web host gives one).
    wall: Option<WallClock>,
    /// The names the unmerged copies were given.
    copy_names: crate::unmerged::Names,
    /// Notices gathered and not taken yet ([`GridLive::take_notices`]).
    told: Vec<String>,
    /// An ordered edit was asked for and its run has not ended (asked, waiting, granted, running
    /// or flushing): the structure lock is, or will be, held by this link.
    ordered_open: bool,
    /// That edit ran (its `finish` ends it once its edits are confirmed).
    ordered_ran: bool,
    /// A renewed ticket held back while an ordered edit is open, and since when.
    ticket_waiting: Option<(String, f64)>,
    /// A renewed ticket went to the hub while connected: the hub switches links once the new one
    /// joined. Until then (or [`SWITCH_WAIT_MS`]) a granted edit does not run.
    switch_since: Option<f64>,
}

/// How long a renewed ticket waits for an open ordered edit to end before it goes to the hub
/// anyway. The page renews 120 s before the old ticket expires, so this leaves a minute.
const TICKET_HOLD_MS: f64 = 60_000.0;
/// How long a granted edit waits for a pending link switch (the new link's join).
const SWITCH_WAIT_MS: f64 = 10_000.0;

type WallClock = Box<dyn FnMut() -> chrono::NaiveDateTime + Send>;

const NOT_ACTIVE: &str = "The live workbook is not open any more";
/// Told when the replica cannot catch up and the file has to be reopened.
pub const RESYNC_TEXT: &str = "This window fell out of step with the other editors. Reopen the file to keep editing.";
/// A cut was cancelled because what it took changed: another editor inserted or deleted a row
/// or column, or wrote into the block, or the window was rebuilt from the room. Pasting it would
/// move the wrong cells or wipe their entry.
pub const CUT_CANCELLED_TEXT: &str = "Your cut was cancelled because the cells it took were changed or moved. Cut again to move them.";
/// Says [`CUT_CANCELLED_TEXT`] once, however many ways the cut was cancelled in one frame.
pub(crate) fn tell_cut_cancelled(notices: &mut Vec<String>) {
    if !notices.iter().any(|n| n == CUT_CANCELLED_TEXT) {
        notices.push(CUT_CANCELLED_TEXT.to_owned());
    }
}

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
        let (wb, seed) = crate::seed::open_base(base)?;
        let (hub, joins) = Tracked::new(hub);
        let core = LiveEditing::<Tracked<H>, Grid>::start(hub, room, &seed, access).map_err(LiveFail::Live)?;
        let title = std::path::Path::new(base.name).file_name().map_or_else(|| base.name.to_owned(), |n| n.to_string_lossy().into_owned());
        let map = crate::seed::seat(session, wb, &seed, title, uid, hash, base.name)?;
        let doc = map.doc;
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
            rejoined_in_run: false,
            run_resets: 0,
            run_told: false,
            lost_pending: None,
            clock: None,
            started: None,
            frames: 0,
            broken_sent: false,
            room: room.to_owned(),
            wall: None,
            copy_names: crate::unmerged::Names::default(),
            told: Vec::new(),
            ordered_open: false,
            ordered_ran: false,
            ticket_waiting: None,
            switch_since: None,
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
        // A reset the app's hook did not see: nothing of the old undo state may run on the new sheet.
        // Today only the test hook moves the count without the hook: a guard, not a real path.
        self.map.clear_undo_for_resets();
        if self.core.live().unconfirmed_edits() == 0 {
            // Everything this window wrote has been ordered: no delete can have missed it.
            self.map.recent.clear();
        }
        let cut = session.clipboard.as_ref().is_some_and(|c| c.cut && c.doc_uid == self.map.doc);
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
        // The other editors' changes are in: a cut they made stale was cancelled. Checked before
        // this window's own ordered run, whose insert or delete ends cut mode as any local one does.
        if cut && !session.clipboard.as_ref().is_some_and(|c| c.cut && c.doc_uid == self.map.doc) {
            tell_cut_cancelled(&mut self.map.notices);
        }
        self.drive_ordered(session);
        let now = self.now();
        self.hand_ticket(now);
        self.after_pump(session);
        self.stamp_unmerged();
        peers
    }

    /// A copy handed over in this pump gets the time it was made, for its name.
    fn stamp_unmerged(&mut self) {
        if let Some(u) = self.map.unmerged.as_mut()
            && u.at.is_none()
        {
            u.at = self.wall.as_mut().map(|f| f()).or_else(crate::unmerged::local_now);
        }
    }

    /// The copies of this replica's local edits that craft-live was about to drop (a run taken
    /// back, a resync, damage, a replaced or a refused replica, or a session ended with edits the
    /// room never confirmed): the base with them as `.xlsx`, named uniquely
    /// ([`crate::unmerged_name`]). Empty when none waits. Taken once; every hand-over since the
    /// last take is in it. Hosts call this after every pump, before [`GridLive::take_notices`].
    ///
    /// The person is told only once a copy exists: [`crate::UNMERGED_TEXT`], or for a run taken
    /// back its own notice, which the host gives the copy's Save action. A kept state that builds
    /// into no workbook comes back as a recovery file ([`crate::RECOVERY_TEXT`]): nothing kept is
    /// ever dropped.
    pub fn take_unmerged(&mut self) -> Vec<crate::UnmergedCopy> {
        self.gather_notices();
        self.stamp_unmerged();
        let Some(u) = self.map.unmerged.take() else { return Vec::new() };
        let built = crate::unmerged::build(&self.map.base, &u.states);
        let mut copies = Vec::new();
        for bytes in built.workbooks {
            let name = self.copy_names.next(&self.map.name, u.at, "xlsx");
            copies.push(crate::UnmergedCopy { name, bytes, taken_back: u.taken_back, recovery: false });
        }
        if !copies.is_empty() && !u.taken_back {
            self.told.push(crate::unmerged::UNMERGED_TEXT.to_owned());
        }
        for bytes in built.unbuilt {
            let name = self.copy_names.next(&self.map.name, u.at, "yrs");
            copies.push(crate::UnmergedCopy { name, bytes, taken_back: u.taken_back, recovery: true });
        }
        if copies.iter().any(|c| c.recovery) {
            self.told.push(crate::unmerged::RECOVERY_TEXT.to_owned());
        }
        copies
    }

    /// Keeps `state` as if craft-live had handed it over (an unbuildable state, for the test that
    /// it does not sink the others).
    #[cfg(feature = "test-util")]
    #[doc(hidden)]
    pub fn keep_unmerged_state(&mut self, state: Vec<u8>) {
        self.map.keep_unmerged(state);
    }

    /// Whether a copy of unmerged edits waits for [`GridLive::take_unmerged`].
    #[must_use]
    pub fn has_unmerged(&self) -> bool {
        self.map.unmerged.is_some()
    }

    /// The person's local date and time, for the unmerged copy's name (a browser host passes
    /// the page's; native hosts have the system's by default).
    pub fn set_wall_clock(&mut self, clock: WallClock) {
        self.wall = Some(clock);
    }

    /// Local edits the room has not confirmed, kept as the unmerged copy before this session goes,
    /// whatever the link's state: an edit sent while connected may still wait for the next chunk
    /// of its ordered run, or for a retry after a refusal, and goes with the session. A copy that
    /// also landed is harmless; a lost one is not. Only a broken link keeps nothing more
    /// (craft-live handed its edits over at the break), and a viewer has none.
    fn keep_unconfirmed(&mut self) {
        if matches!(self.core.status(), LiveStatus::Broken(_) | LiveStatus::ReadOnly) || self.core.live().unconfirmed_edits() == 0 {
            return;
        }
        if let Some(state) = self.core.live().doc().map(GridDoc::state) {
            self.map.keep_unmerged(state);
        }
    }

    /// Ends the link for `reason` when the host says the session is over (the page reopened the
    /// file in another room, or the ticket lost edit access). Queued edits are flushed first, and
    /// every edit the room has not confirmed is kept as the unmerged copy, whatever the status
    /// says: the room is being left, so what was sent to it may never be ordered. Only a link
    /// that is already Broken keeps nothing more: craft-live handed its edits over at that break.
    /// Hosts take the copy with [`GridLive::take_unmerged`].
    pub fn end(&mut self, session: &mut Session, reason: &str) {
        if matches!(self.core.status(), LiveStatus::Broken(_)) {
            return;
        }
        if session.document_index(self.map.doc).is_some() {
            self.flush(session);
        }
        self.keep_unconfirmed();
        self.core.end(reason);
        self.stamp_unmerged();
    }

    fn after_pump(&mut self, session: &Session) {
        // A reset the app's hook did not see still moved the count: the undo state clears here.
        // Today only the test hook moves the count without the hook: a guard, not a real path.
        self.map.clear_undo_for_resets();
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

    /// An ordered undo or redo that never ran (nothing was written) goes back on its stack,
    /// unless the sheet was reloaded since it was taken off: then the history it belonged to was
    /// cleared (and the user told), so it is dropped.
    fn replay_unrun(&mut self, p: &Pending) {
        if let Some(r) = &p.undo {
            if !self.replay_stale(r) {
                self.map.undo.put_back(r.step.clone(), r.redo);
            }
            self.replay_over();
        }
    }

    /// The replay was taken off its stack before the last reset: its keys and ops belong to the
    /// replaced replica.
    fn replay_stale(&self, r: &crate::undo::Replay) -> bool {
        r.resets != self.map.resets
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
        // A reset the app's hook did not see: a stale undo must not run on the rebuilt sheet.
        self.map.clear_undo_for_resets();
        match class(id) {
            Class::Local => {
                self.activate(session)?;
                if matches!(id, "file.close" | "window.close") {
                    // Edits queued in this frame become updates while the workbook is still open:
                    // after the close nothing can read them.
                    self.flush(session);
                }
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
                self.ordered_open = true;
                self.ordered_ran = false;
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
                self.rejoined_in_run = true;
                // The relay gave the lock back with the old link, so a run that has not started
                // would be refused: it is dropped here, and `finish` gives the grant back.
                if let Some(p) = self.deferred.take() {
                    self.replay_unrun(&p);
                    self.run_told = true;
                    self.map.notices.push(LOCK_LOST_UNRUN_LINK_TEXT.to_owned());
                }
            }
            self.seen_joins = joins;
            self.switch_since = None;
        }
        if self.switch_since.is_some_and(|since| now - since >= SWITCH_WAIT_MS) {
            // The new link never joined (refused, or another room): the hub kept the old one.
            self.switch_since = None;
        }
        if self.switch_since.is_some() {
            // A grant on the link about to close would be freed with it: wait for the new one.
            self.finish_ordered();
            return;
        }
        let polled = self.ordered.poll(self.core.newest_seq(), now);
        let polled = match polled {
            Polled::Nothing => self.deferred.take().map_or(Polled::Nothing, Polled::Run),
            other => other,
        };
        match polled {
            Polled::Nothing => {}
            Polled::Run(p) => {
                self.ordered_ran = true;
                self.rejoined_in_run = false;
                self.run_told = false;
                self.run_resets = self.map.resets;
                self.run_pending(session, p);
            }
            Polled::GaveUp { holder, item } => {
                self.ordered_open = false;
                self.replay_unrun(&item);
                let who = self.holder_name(&holder);
                self.map.notices.push(format!("{who} is still changing the sheet's structure. Your change was not made; try again in a moment."));
            }
            Polled::LockLost { .. } => {
                if !self.ordered_ran {
                    // Lost before it ran: nothing is left to unlock or wait for.
                    self.ordered_open = false;
                }
                if let Some(p) = self.deferred.take() {
                    // It never ran, but free edits made while it waited were under the lock too.
                    self.replay_unrun(&p);
                    if !self.run_told {
                        let text = if self.rejoined_in_run { LOCK_LOST_UNRUN_LINK_TEXT } else { LOCK_LOST_UNRUN_TEXT };
                        self.map.notices.push(text.to_owned());
                    }
                } else if matches!(self.core.status(), LiveStatus::Broken(_)) {
                    // The replica broke: the relay never answered for the edit.
                    if !self.run_told {
                        self.map.notices.push(LOCK_LOST_BROKEN_TEXT.to_owned());
                    }
                } else {
                    // Whether the edit was kept or taken back is only known once the run has
                    // settled (the relay's answer to its last submit): told below, once.
                    self.lost_pending = Some((self.run_resets, self.rejoined_in_run));
                }
            }
        }
        if let Some((resets, rejoined)) = self.lost_pending {
            let moved = self.map.resets != resets;
            if self.run_told {
                // A dropped edit was told when it was dropped; edits typed meanwhile that were
                // taken back were warned of there.
                self.lost_pending = None;
            } else if moved || self.core.local_settled() {
                self.lost_pending = None;
                let text = if !moved {
                    // Nothing was taken back: the lock report came after the edits were ordered.
                    LOCK_LOST_KEPT_TEXT
                } else if rejoined {
                    LOCK_LOST_LINK_TEXT
                } else {
                    // The relay refused the edit; the link took it out of the replica, and
                    // `LiveApp::reset` and `resync` put the engine back at the room's state.
                    LOCK_LOST_TEXT
                };
                self.map.notices.push(text.to_owned());
            }
        }
        self.finish_ordered();
    }

    /// Unlocks once the run's edits settled (`Ordered::finish`, every frame until it does), and
    /// notes that the ordered edit ended: its state then goes back to idle.
    fn finish_ordered(&mut self) {
        if self.deferred.is_none() {
            let settled = self.core.local_settled();
            if self.broken_sent || (self.ordered_ran && settled) {
                self.ordered_open = false;
                self.ordered_ran = false;
            }
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
        if !self.map.pending.is_empty() || self.map.restore.is_some() || session.in_group(self.map.doc) {
            // The engine has not applied everything the room holds, or an undo group is open.
            self.deferred = Some(p);
            return;
        }
        if let Err(e) = self.activate(session) {
            self.map.notices.push(e);
            self.replay_unrun(&p);
            return;
        }
        if let Some(r) = &p.undo {
            if self.replay_stale(r) {
                // The sheet was reloaded while it waited: the history was cleared, so it never runs.
                self.replay_over();
                return;
            }
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
        self.gather_notices();
        std::mem::take(&mut self.told)
    }

    /// Moves what this crate and the link have to say into `told`, and marks a waiting copy as a
    /// run taken back when a notice says one was.
    fn gather_notices(&mut self) {
        for n in std::mem::take(&mut self.map.notices) {
            self.core.push_notice(n);
        }
        let mut notices = self.core.take_notices();
        // An ordered run taken back: the link words it in terms of its scope (it names `SCOPE`);
        // this crate says it for a user, once, in `drive_ordered`. When nothing of ours explains
        // it (a lock lost while still waiting), say that edits were taken back.
        let before = notices.len();
        notices.retain(|n| !n.contains(SCOPE));
        if notices.len() < before && self.lost_pending.is_none() && !self.run_told {
            let told = notices.iter().any(|n| {
                [LOCK_LOST_TEXT, LOCK_LOST_LINK_TEXT, LOCK_LOST_BROKEN_TEXT, LOCK_LOST_UNRUN_TEXT, LOCK_LOST_UNRUN_LINK_TEXT, TAKEN_BACK_TEXT]
                    .contains(&n.as_str())
            });
            if !told {
                notices.push(TAKEN_BACK_TEXT.to_owned());
            }
        }
        if let Some(u) = self.map.unmerged.as_mut()
            && notices.iter().any(|n| crate::unmerged::TAKE_BACK_TEXTS.contains(&n.as_str()))
        {
            u.taken_back = true;
        }
        self.told.extend(notices);
    }

    /// Moves the reset count without the app's reset hook, as a rebuild the hook does not see
    /// would. For tests only.
    #[cfg(any(test, feature = "test-util"))]
    #[doc(hidden)]
    pub fn count_reset_without_hook(&mut self) {
        self.map.resets = self.map.resets.saturating_add(1);
    }

    /// Notes a line another peer deleted under this window's entry, as a remote delete does, for
    /// the next frame to tell. For tests only.
    #[cfg(any(test, feature = "test-util"))]
    #[doc(hidden)]
    pub fn note_removed(&mut self, by: &str, cell: &str, value: &str, row: bool) {
        self.map.removed.push((by.to_owned(), cell.to_owned(), value.to_owned(), if row { "row" } else { "column" }));
    }

    /// How many times the replica was replaced under this window (a refused run taken back, the
    /// room's checkpoint). The undo log must clear its steps when this changes: every local edit
    /// that was not ordered is out of the document.
    #[must_use]
    pub fn resets(&self) -> u32 {
        self.map.resets
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
        if let Some(p) = crate::presence::payload(&self.map.layout, d.wb.active_sheet(), sel.active, &sel.ranges, editing) {
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

    /// The room this session joined.
    #[must_use]
    pub fn room(&self) -> &str {
        &self.room
    }

    /// The engine document (`DocState::uid`) this session edits.
    #[must_use]
    pub fn doc(&self) -> u64 {
        self.map.doc
    }

    pub fn set_clock(&mut self, clock: Box<dyn FnMut() -> f64 + Send>) {
        let shared = Arc::new(Mutex::new(clock));
        let link = shared.clone();
        self.core.set_clock(Box::new(move || link.lock().map_or(0.0, |mut f| f())));
        self.clock = Some(shared);
    }

    /// A renewed ticket from the host (the page's ticket refresh), for the hub.
    ///
    /// With a hub that switches links on a new ticket (`craft_collab::Handover`, as on the web)
    /// the switch is a rejoin, and the relay frees the structure lock with the old link. So
    /// while an ordered (row or column) edit is asked, waiting or running, the ticket is held and
    /// handed over on the first frame after that edit ended ([`TICKET_HOLD_MS`] at most), and a
    /// lock granted while the switch is pending is not used until the new link joined: the
    /// rejoin then asks again. A ticket for a link that is down goes over at once (it is the one
    /// the reconnect needs).
    pub fn renew_ticket(&mut self, ticket: String) {
        let now = self.now();
        self.ticket_waiting = Some((ticket, now));
        self.hand_ticket(now);
    }

    /// Hands a held ticket to the hub when nothing holds it back.
    fn hand_ticket(&mut self, now: f64) {
        let Some((_, since)) = &self.ticket_waiting else { return };
        let connected = self.core.hub().connected();
        if self.ordered_open && connected && now - since < TICKET_HOLD_MS {
            return;
        }
        if let Some((ticket, _)) = self.ticket_waiting.take() {
            self.core.hub_mut().set_token(ticket);
            if connected {
                self.switch_since = Some(now);
            }
        }
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

    /// Ends the session: queued edits are shared, every edit the room has not confirmed is kept,
    /// the window leaves the room by name (so the other editors stop drawing it at once), and the
    /// engine's observer, undo hook and random source come off, so the workbook is an ordinary
    /// one again. Returns the unmerged copies ([`GridLive::take_unmerged`]): the host saves them,
    /// as any other copy.
    #[must_use = "the unmerged copies hold edits the other editors never got"]
    pub fn stop(mut self, session: &mut Session) -> Vec<crate::UnmergedCopy> {
        // Queued edits go out first (a close runs through `run`, which flushes before it), then
        // what the room lacks is kept, and only then is the room left.
        if session.document_index(self.map.doc).is_some() {
            self.flush(session);
        }
        self.keep_unconfirmed();
        let _ = self.core.hub_mut().send(ClientMsg::Leave { doc: self.room.clone() });
        session.set_observer(None);
        if session.live_undo_doc() == Some(self.map.doc) {
            // `set_live_undo_hook_for` refuses a closed document; this clears the hook either way.
            session.set_live_undo_hook(None);
        }
        if session.document_index(self.map.doc).is_some() {
            // Clearing a source cannot fail for a document that is open.
            let _ = session.set_volatile_source(self.map.doc, None);
        }
        self.take_unmerged()
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
