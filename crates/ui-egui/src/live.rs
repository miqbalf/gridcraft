//! Live co-editing in the window.
//!
//! The host starts a [`gridcraft_live::GridLive`] on the window's workbook and hands it in
//! through [`crate::Services::live`]. From then on the window
//!
//! - pumps it once per frame ([`SheetApp::logic`] calls `live_frame`), which takes in the other
//!   editors' changes and carets;
//! - runs every command through [`gridcraft_live::GridLive::run`] ([`SheetApp::engine_run`]), so
//!   the gate decides what is shared, queued behind the structure lock, or refused;
//! - greys the ribbon commands the gate refuses, with the reason as the tooltip;
//! - shows the link's status, who the window waits for, and the room's notices;
//! - keeps an open cell editor on its cell when rows or columns move (it is anchored to the
//!   row and column ids, as in Google Sheets), and keeps the typed text when its row is deleted.
//!
//! Drawing the other editors' selections is [`crate::grid::presence`].

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use craft_collab::{ClientMsg, Hub, ServerMsg};
use craft_live::LiveStatus;
use craft_ui::presence::{Peer as Badge, PresenceBar, PresenceLabels, colours_for};
use egui::Color32;
use gridcraft_live::{GridLive, GridPos, NOT_YET_TEXT, Peer, Ran};
use serde_json::{Value as Json, json};

use crate::i18n::Language;
use crate::{Notice, NoticeKind, SheetApp};

/// A boxed hub: what a host that picks its transport at run time hands to the session.
///
/// `craft_collab` does not implement [`Hub`] for `Box<dyn Hub>`, so this wraps it.
pub struct DynHub(pub Box<dyn Hub>);

impl Hub for DynHub {
    fn send(&mut self, msg: ClientMsg) -> craft_collab::Result<()> {
        self.0.send(msg)
    }

    fn poll(&mut self) -> Vec<ServerMsg> {
        self.0.poll()
    }

    fn connected(&self) -> bool {
        self.0.connected()
    }

    fn set_token(&mut self, token: String) {
        self.0.set_token(token);
    }
}

/// The session this window runs, over a boxed hub.
pub type Live = GridLive<DynHub>;

/// How long a peer's name tag stays after its cell stopped moving, in seconds.
pub const TAG_SECS: f64 = 3.0;

/// How often a live window polls the link when nothing else wakes it.
const POLL: Duration = Duration::from_millis(100);

/// Told when the cell being edited was deleted by another editor; `{draft}` is what was typed.
pub const EDITOR_GONE_TEXT: &str = "The row or column of the cell you were editing was deleted by another editor.";
/// The same, with the entry kept.
pub const EDITOR_GONE_DRAFT_TEXT: &str = "The row or column of the cell you were editing was deleted by another editor. Your entry was: {draft}";
/// Told when the live workbook is closed.
pub const LIVE_ENDED_TEXT: &str = "Live editing ended because the workbook was closed.";
/// Longest draft the info bar shows; a longer one is also shown whole in a message box.
const DRAFT_SHOWN: usize = 160;

const BLOCKED_ID: &str = "gridcraft.live.blocked";
/// Most notices kept for the info bar; past it the oldest is dropped.
const QUEUE_MAX: usize = 20;

/// Told when a session is attached to a window that already has one.
pub const ALREADY_LIVE_TEXT: &str = "This window is already editing live";
/// Told when the session's workbook is not open in this window.
pub const ATTACH_NO_DOC_TEXT: &str = "The live session was started on a workbook this window does not have open";
/// Told when the session was started on another engine session.
pub const ATTACH_OTHER_SESSION_TEXT: &str = "The live session was started on another engine session";
/// Shown when the host's session was refused at start; `{why}` is one of the reasons above.
pub const NOT_STARTED_TEXT: &str = "Live editing was not started. {why}";

/// The ribbon controls the gate refuses, as the ribbon names them.
#[derive(Clone, Default)]
struct Blocked {
    /// Plain buttons, by the English name of their command (the catalog label).
    labels: Arc<BTreeSet<String>>,
    /// Split buttons and menus, by the command their main part or gallery runs: a label shared
    /// with another command (a "Clear" menu of allowed items) must not grey them.
    menus: Arc<BTreeSet<String>>,
    /// Split buttons whose main part runs a refused command but whose menu holds other things.
    mains: Arc<BTreeSet<String>>,
}

/// The split buttons and menus whose command is not shared, by control name and the command id
/// the control runs (its main part, or every item of its gallery).
const MENU_COMMANDS: &[(&str, &str)] = &[
    ("Add Chart Element", "chart.addElement"),
    ("Column or Bar", "insert.chart"),
    ("Date & Time", "formulas.insertFunction"),
    ("Financial", "formulas.insertFunction"),
    ("Insert a picture", "insert.picture"),
    ("Logical", "formulas.insertFunction"),
    ("Lookup & Reference", "formulas.insertFunction"),
    ("Math & Trig", "formulas.insertFunction"),
    ("More Functions", "formulas.insertFunction"),
    ("Paper Size", "pageLayout.size"),
    ("Recently Used", "formulas.insertFunction"),
    ("Text", "formulas.insertFunction"),
    ("Hierarchy & more", "insert.chart"),
    ("Insert an equation, or pick one from the gallery", "insert.equation"),
    ("Insert an icon", "insert.icons"),
    ("Line or Area", "insert.chart"),
    ("Pie or Doughnut", "insert.chart"),
    ("Scatter or Bubble", "insert.chart"),
    ("Statistic", "insert.chart"),
    ("Calculation Options", "formulas.calculationOptions"),
    ("Change Chart Type", "chart.changeType"),
    ("Conditional Formatting", "home.conditionalFormat"),
    ("Decorative Text", "insert.wordArt"),
    ("Format as Table", "home.formatAsTable"),
    ("Freeze Panes", "view.freezePanes"),
    ("Margins", "pageLayout.margins"),
    ("Merge & Center", "home.mergeCenter"),
    ("Page Layout|Orientation", "pageLayout.orientation"),
    ("Print Area", "pageLayout.printArea"),
    ("Shapes", "insert.shape"),
    ("Sparklines", "insert.sparkline"),
    ("Symbol", "insert.symbol"),
    ("Themes", "pageLayout.theme"),
    ("Use in Formula", "formulas.useInFormula"),
];

/// Split buttons whose main part runs a refused command while the menu under the arrow holds other
/// things (the Macros dialog): only the main part is greyed.
const MAIN_COMMANDS: &[(&str, &str)] = &[("Run a macro on the selection", "macro.run")];

/// Dropdowns whose popup is `menu_items` (it greys each item by its own command) or lists
/// commands one by one, so the control itself stays enabled.
const PER_ITEM_TIPS: &[&str] = &[
    "Accounting Number Format",
    "Clear",
    "Data Validation",
    "Delete",
    "Fill",
    "Find & Select",
    "Format",
    "Insert",
    "Notes",
    "Orientation",
    "Page Breaks",
    "Paste",
    "Sort & Filter",
    "Underline",
    "What-If Analysis",
];

/// Dropdowns and galleries that run no command the gate refuses (dialogs, pickers).
const OPEN_TIPS: &[&str] =
    &["AutoSum", "Borders", "Cell Styles", "Error Checking", "Fill Color", "Font Color", "Share: export or save a copy", "Switch Windows"];

const SEEN_MENUS_ID: &str = "gridcraft.live.menus-seen";

/// Notes that a dropdown, gallery or split button called `tip` was drawn.
pub(crate) fn note_menu_tip(ctx: &egui::Context, tip: &str) {
    ctx.data_mut(|d| {
        let seen = d.get_temp_mut_or_default::<Arc<std::sync::Mutex<BTreeSet<String>>>>(egui::Id::NULL.with(SEEN_MENUS_ID));
        if let Ok(mut set) = seen.lock() {
            set.insert(tip.to_owned());
        }
    });
}

/// The dropdowns, galleries and split buttons drawn so far in this context, by tooltip (for the
/// test that every one of them is classified for the live gate).
#[doc(hidden)]
#[must_use]
pub fn menu_tips_seen(ctx: &egui::Context) -> Vec<String> {
    let seen = ctx.data_mut(|d| d.get_temp::<Arc<std::sync::Mutex<BTreeSet<String>>>>(egui::Id::NULL.with(SEEN_MENUS_ID)));
    seen.and_then(|s| s.lock().ok().map(|set| set.iter().cloned().collect())).unwrap_or_default()
}

/// How the live gate treats the dropdown, gallery or split button `tip`: `Some(Some(id))` greys it
/// whole by that command, `Some(None)` leaves it to its items or to nothing refused, `None` when
/// the gate does not know the control (a new one must be added to a table in this file).
#[doc(hidden)]
#[must_use]
pub fn menu_tip_class(tip: &str) -> Option<Option<&'static str>> {
    if let Some((_, id)) = MENU_COMMANDS.iter().chain(MAIN_COMMANDS).find(|(t, _)| *t == tip) {
        return Some(Some(id));
    }
    (PER_ITEM_TIPS.contains(&tip) || OPEN_TIPS.contains(&tip)).then_some(None)
}

/// Every control the tables of this file name, for the test that none is stale.
#[doc(hidden)]
#[must_use]
pub fn menu_tables() -> Vec<(&'static str, Option<&'static str>)> {
    let mut all: Vec<_> = MENU_COMMANDS.iter().chain(MAIN_COMMANDS).map(|(t, id)| (*t, Some(*id))).collect();
    all.extend(PER_ITEM_TIPS.iter().chain(OPEN_TIPS).map(|t| (*t, None)));
    all
}

/// A live window's state: the session, who else is here, and what the presence drawing needs.
pub struct LiveUi {
    live: Live,
    /// The workbook the session edits (`DocState::uid`).
    doc: u64,
    peers: Vec<Peer>,
    bar: PresenceBar,
    /// Where each peer's active cell was last seen, and when it moved there (egui time).
    moved: HashMap<String, (GridPos, f64)>,
    /// Peers known to be read-only (the roster does not say).
    viewers: HashSet<String>,
    /// Ribbon names of the commands the gate refuses.
    blocked: Blocked,
    /// The notice the info bar shows now: its text without the count, and as shown.
    shown: Option<(String, String)>,
    /// Notices not shown yet, oldest first: the info bar shows one at a time.
    queue: VecDeque<(String, NoticeKind)>,
}

impl LiveUi {
    /// Wraps a started session. The workbook it edits is the session's own.
    #[must_use]
    pub fn new(live: Live) -> LiveUi {
        let doc = live.doc();
        let blocked = Blocked {
            labels: Arc::new(blocked_names(&live)),
            menus: Arc::new(MENU_COMMANDS.iter().filter(|(_, id)| !live.allows(id)).map(|(tip, _)| (*tip).to_owned()).collect()),
            mains: Arc::new(MAIN_COMMANDS.iter().filter(|(_, id)| !live.allows(id)).map(|(tip, _)| (*tip).to_owned()).collect()),
        };
        LiveUi {
            live,
            doc,
            peers: Vec::new(),
            bar: PresenceBar::new(),
            moved: HashMap::new(),
            viewers: HashSet::new(),
            blocked,
            shown: None,
            queue: VecDeque::new(),
        }
    }

    #[must_use]
    pub fn live(&self) -> &Live {
        &self.live
    }

    pub fn live_mut(&mut self) -> &mut Live {
        &mut self.live
    }

    /// The other editors, as of the last frame.
    #[must_use]
    pub fn peers(&self) -> &[Peer] {
        &self.peers
    }

    /// Marks which peers (by peer id) only read. The roster carries no access, so the host says,
    /// when it knows; those peers are labelled "viewing".
    pub fn set_viewers(&mut self, ids: impl IntoIterator<Item = String>) {
        self.viewers = ids.into_iter().collect();
    }

    /// Whether the peer with this id is known to be read-only.
    #[must_use]
    pub fn is_viewer(&self, id: &str) -> bool {
        self.viewers.contains(id)
    }

    /// The colour of each peer, in [`LiveUi::peers`] order: the one its badge has, so the badge
    /// and the outline on the sheet always agree (colours are made distinct across the room).
    #[must_use]
    pub fn colours(&self) -> Vec<Color32> {
        let ids: Vec<String> = self.peers.iter().map(|p| p.id.clone()).collect();
        colours_for(&ids)
    }

    /// Whether the peer's name tag is shown: its cell moved in the last [`TAG_SECS`].
    #[must_use]
    pub fn tag_recent(&self, id: &str, now: f64) -> bool {
        self.moved.get(id).is_some_and(|(_, at)| now - at < TAG_SECS)
    }

    /// The colours of the peers whose cell is on `sheet` while the window shows `shown` (the dots
    /// on a sheet's tab are for the people working on a sheet nobody is looking at).
    #[must_use]
    pub fn tab_dots(&self, sheet: usize, shown: usize) -> Vec<Color32> {
        if sheet == shown {
            return Vec::new();
        }
        let colours = self.colours();
        self.peers.iter().zip(colours).filter(|(p, _)| p.caret.as_ref().is_some_and(|c| c.sheet == sheet)).map(|(_, c)| c).collect()
    }

    /// The badges for the presence bar, in the interface language.
    #[must_use]
    pub fn badges(&self, lang: Language, sheet_name: &dyn Fn(usize) -> String) -> Vec<Badge> {
        self.peers
            .iter()
            .map(|p| {
                let viewing = self.viewers.contains(&p.id);
                let display = if viewing { lang.tr_fill("{name} (viewing)", &[("name", &p.display)]) } else { p.display.clone() };
                let mut badge = Badge::new(p.id.clone(), display);
                if let Some(c) = &p.caret {
                    badge = badge.at(format!("{} · {}", sheet_name(c.sheet), c.active.a1()));
                }
                badge
            })
            .collect()
    }

    /// The link's status in words, and whether it is a problem.
    #[must_use]
    pub fn status_text(&self, lang: Language) -> (String, bool) {
        match self.live.status() {
            LiveStatus::Connecting => (lang.tr("Connecting…").to_owned(), false),
            LiveStatus::Live { saved_at: None } => (lang.tr("Live").to_owned(), false),
            LiveStatus::Live { saved_at: Some(at) } => (lang.tr_fill("Live, saved {time}", &[("time", at)]), false),
            LiveStatus::ReadOnly => (lang.tr("Viewing only").to_owned(), false),
            LiveStatus::Reconnecting => (lang.tr("Reconnecting…").to_owned(), true),
            LiveStatus::Refused(m) | LiveStatus::Broken(m) => (lang.tr(m).to_owned(), true),
        }
    }

    /// "Waiting for Bob's change…" while an ordered edit waits for another editor's lock.
    #[must_use]
    pub fn status_line(&self, lang: Language) -> Option<String> {
        self.live.status_line().map(|line| lang.tr(&line).to_owned())
    }

    /// Queues a notice for the info bar; past [`QUEUE_MAX`] the oldest goes.
    fn push_notice(&mut self, text: String, kind: NoticeKind) {
        if self.queue.len() >= QUEUE_MAX {
            self.queue.pop_front();
        }
        self.queue.push_back((text, kind));
    }

    /// Takes in the roster after a pump: tracks who moved, and refreshes the presence bar.
    fn refresh(&mut self, mut peers: Vec<Peer>, now: f64, lang: Language, names: &dyn Fn(usize) -> String) {
        // A caret on an id this window does not know yet is not drawn until it is known.
        for p in &mut peers {
            p.caret = p.caret.take().filter(|c| !c.unknown);
        }
        self.moved.retain(|id, _| peers.iter().any(|p| &p.id == id));
        for p in &peers {
            let Some(c) = &p.caret else { continue };
            match self.moved.get_mut(&p.id) {
                Some((at, since)) if *at != *c => {
                    *at = c.clone();
                    *since = now;
                }
                Some(_) => {}
                None => {
                    self.moved.insert(p.id.clone(), (c.clone(), now));
                }
            }
        }
        self.peers = peers;
        let labels = PresenceLabels {
            alone: String::new(),
            with_one: lang.tr("Editing with {name}").to_owned(),
            with_many: lang.tr("Editing with {count} others").to_owned(),
            and_more: lang.tr("{count} more").to_owned(),
            who: lang.tr("Who is here").to_owned(),
        };
        let bar = std::mem::take(&mut self.bar).with_labels(labels);
        self.bar = bar;
        let badges = self.badges(lang, names);
        self.bar.set(badges);
    }
}

/// What is wrong with attaching `live` to `session`, as the English text that `tr` translates.
fn attach_problem(session: &gridcraft_engine::Session, live: &Live) -> Option<&'static str> {
    let doc = live.doc();
    if session.document_index(doc).is_none() {
        return Some(ATTACH_NO_DOC_TEXT);
    }
    (session.live_undo_doc() != Some(doc)).then_some(ATTACH_OTHER_SESSION_TEXT)
}

/// Whether `live` was started on `session`: its workbook is open there and carries the session's
/// edit hook. A session started elsewhere would let this window edit with nothing shared.
///
/// # Errors
/// What is wrong, as text for the user.
pub fn check_attached(session: &gridcraft_engine::Session, live: &Live, lang: Language) -> Result<(), String> {
    attach_problem(session, live).map_or(Ok(()), |why| Err(lang.tr(why).to_owned()))
}

/// The window's start-up refusal of a live session, as the key to translate once the user's language
/// is known: `SheetApp::new` runs before the preferences load.
pub(crate) fn startup_refusal(session: &gridcraft_engine::Session, live: &Live) -> Option<&'static str> {
    attach_problem(session, live)
}

/// The ribbon names (catalog labels) of the commands the gate refuses. A label shared with a
/// command that is allowed stays enabled: the ribbon cannot tell them apart.
fn blocked_names(live: &Live) -> BTreeSet<String> {
    let mut all_blocked: BTreeMap<&str, bool> = BTreeMap::new();
    for (id, _tab, _group, label) in gridcraft_engine::catalog::CATALOG {
        let blocked = !live.allows(id);
        all_blocked.entry(label).and_modify(|b| *b &= blocked).or_insert(blocked);
    }
    all_blocked.into_iter().filter(|(_, blocked)| *blocked).map(|(label, _)| (*label).to_owned()).collect()
}

/// Whether the ribbon control named `tip` (the English name, as its tooltip is keyed) runs a
/// command the live session refuses; `menu` for a split button or menu, which is looked up by the
/// command it runs. False when the window is not live.
pub(crate) fn tip_blocked(ctx: &egui::Context, tip: &str, menu: bool) -> bool {
    let Some(b) = ctx.data(|d| d.get_temp::<Blocked>(egui::Id::NULL.with(BLOCKED_ID))) else { return false };
    if menu { b.menus.contains(tip) } else { b.labels.contains(&tip.replace('\n', " ")) }
}

/// Whether only the main part of the split button `tip` is refused.
pub(crate) fn tip_main_blocked(ctx: &egui::Context, tip: &str) -> bool {
    ctx.data(|d| d.get_temp::<Blocked>(egui::Id::NULL.with(BLOCKED_ID))).is_some_and(|b| b.mains.contains(tip))
}

/// The tooltip of a greyed control.
pub(crate) fn blocked_tip(ctx: &egui::Context) -> String {
    crate::i18n::current(ctx).tr(NOT_YET_TEXT).to_owned()
}

impl SheetApp {
    /// Tells why a greyed control did not open: the live session refuses what it runs.
    pub(crate) fn tell_control_refused(&mut self, ctx: &egui::Context) {
        self.notice = Some(Notice { text: blocked_tip(ctx), kind: NoticeKind::Info, action: None });
    }

    /// The live session, if the window is in one.
    #[must_use]
    pub fn live(&self) -> Option<&LiveUi> {
        self.live.as_ref()
    }

    pub fn live_mut(&mut self) -> Option<&mut LiveUi> {
        self.live.as_mut()
    }

    /// Attaches a live session started on the active workbook, for a host that has it only after
    /// the window exists (the web host). The session must have been started on this window's
    /// engine session (it carries the session's edit hook).
    ///
    /// # Errors
    /// The window is already live, or the session was not started on this window's engine session.
    pub fn attach_live(&mut self, live: Live) -> Result<(), String> {
        if self.live.is_some() {
            return Err(self.ui.language.tr(ALREADY_LIVE_TEXT).to_owned());
        }
        check_attached(&self.session, &live, self.ui.language)?;
        self.live = Some(LiveUi::new(live));
        Ok(())
    }

    /// Whether the live session refuses `id`, a command that exists (UI-only ids and dialogs are
    /// never gated). Always false when the window is not live.
    #[must_use]
    pub fn live_gates(&self, id: &str) -> bool {
        self.live.as_ref().is_some_and(|l| !l.live.allows(id)) && gridcraft_engine::find_command(id).is_some()
    }

    /// Why typing is refused: the file is read-only for this window. `None` when it takes edits
    /// (or while it connects, when the entry is held until the link is up).
    pub(crate) fn live_read_only(&self) -> Option<String> {
        let live = self.live.as_ref()?;
        matches!(live.live.status(), LiveStatus::ReadOnly).then(|| self.ui.language.tr(&live.live.core().not_editable()).to_owned())
    }

    /// Runs a command on the workbook: through the live session when there is one (the gate
    /// decides), else on the engine.
    ///
    /// Only a command whose target is the live workbook goes through the session: it makes the
    /// live workbook active, so a command for another open workbook (a new one, a dropped file)
    /// would otherwise be written into the live one and shared. Those run on their own workbook.
    pub(crate) fn engine_run(&mut self, id: &str, params: Json) -> Result<Json, String> {
        let target = self.session.active().map(|d| d.uid);
        match self.live.as_mut().filter(|lu| target == Some(lu.doc)) {
            Some(lu) => match lu.live.run(&mut self.session, id, params)? {
                Ran::Done(done) => Ok(done),
                Ran::Queued => Ok(json!({"queued": true})),
            },
            None => self.session.run(id, params),
        }
    }

    /// Ends the live session (its workbook was closed): the engine's hooks come off, and the
    /// window goes on with its other workbooks.
    fn end_live(&mut self, ctx: &egui::Context, mut lu: LiveUi) {
        // Leave the room by name, so the other editors stop drawing this window at once instead of
        // waiting for the transport to notice.
        let room = lu.live.room().to_owned();
        let _ = lu.live.core_mut().hub_mut().send(ClientMsg::Leave { doc: room });
        let mut told = lu.live.take_notices();
        told.extend(lu.queue.drain(..).map(|(text, _)| text));
        ctx.data_mut(|d| d.remove_temp::<Blocked>(egui::Id::NULL.with(BLOCKED_ID)));
        lu.live.stop(&mut self.session);
        let lang = self.ui.language;
        // What the room told that was not shown yet comes along with the reason it ended.
        let mut text = lang.tr(LIVE_ENDED_TEXT).to_owned();
        for t in told {
            text.push(' ');
            text.push_str(&t);
        }
        self.notice = Some(Notice { text, kind: NoticeKind::Info, action: None });
    }

    /// One frame of a live window: anchors the open editor, takes in the room, moves the editor
    /// with its row, and shows what the room told.
    pub(crate) fn live_frame(&mut self, ctx: &egui::Context) {
        // A live session refused when the window was built is told in the user's language now.
        if let Some(why) = self.live_refusal.take() {
            let lang = self.ui.language;
            self.message = Some(("GridCraft".into(), lang.tr_fill(NOT_STARTED_TEXT, &[("why", lang.tr(why))])));
        }
        let Some(mut lu) = self.live.take() else { return };
        if self.session.document_index(lu.doc).is_none() {
            self.end_live(ctx, lu);
            return;
        }
        let now = ctx.input(|i| i.time);
        let lang = self.ui.language;
        let slot = self.live_editor_slot(lu.doc);
        lu.live.anchor_editor(slot);
        // Only an anchor that resolves can be lost: one that never took (a cell outside the
        // layout) must not close the editor.
        let tracked = slot.is_some() && lu.live.editor_at(&self.session).is_some();
        // Running a queued row or column change makes the live workbook active: put the view back
        // on the workbook the user is in.
        let viewing = self.session.active().map(|d| d.uid);
        let peers = lu.live.pump(&mut self.session);
        if let Some(i) = viewing.filter(|uid| self.session.active().map(|d| d.uid) != Some(*uid)).and_then(|uid| self.session.document_index(uid)) {
            self.session.set_active(i);
        }
        let gone = tracked.then(|| self.follow_live_editor(&lu)).flatten();
        let doc = lu.doc;
        let names = |sheet: usize| {
            self.session.documents().iter().find(|d| d.uid == doc).and_then(|d| d.wb.sheet(sheet)).map(|s| s.name.clone()).unwrap_or_default()
        };
        lu.refresh(peers, now, lang, &names);
        let notes = lu.live.take_notices();
        lu.live.send_selection(&self.session);
        ctx.data_mut(|d| d.insert_temp(egui::Id::NULL.with(BLOCKED_ID), lu.blocked.clone()));
        // A name tag that is showing is due to go: wake up for it.
        let due = lu
            .moved
            .values()
            .map(|(_, at)| at + TAG_SECS - now)
            .filter(|left| *left > 0.0)
            .fold(None, |a: Option<f64>, left| Some(a.map_or(left, |a| a.min(left))));
        ctx.request_repaint_after(due.map_or(POLL, |left| POLL.min(Duration::from_secs_f64(left.max(0.01)))));
        for text in notes {
            lu.push_notice(lang.tr(&text).to_owned(), NoticeKind::Info);
        }
        if let Some(draft) = gone {
            self.tell_editor_gone(&mut lu, draft);
        }
        // One notice at a time in the info bar, the next when it is dismissed; the count of those
        // waiting is kept true as more arrive.
        let with_count = |waiting: usize, text: &str| {
            if waiting == 0 { text.to_owned() } else { lang.tr_fill("{text} (+{count} more)", &[("text", text), ("count", &waiting.to_string())]) }
        };
        let still_ours = lu.shown.as_ref().is_some_and(|(_, full)| self.notice.as_ref().is_some_and(|n| &n.text == full));
        if still_ours {
            let waiting = lu.queue.len();
            if let Some((base, full)) = lu.shown.as_mut() {
                let text = with_count(waiting, base);
                if *full != text
                    && let Some(n) = self.notice.as_mut()
                {
                    n.text.clone_from(&text);
                    *full = text;
                }
            }
        } else {
            lu.shown = None;
            if self.notice.is_none()
                && let Some((text, kind)) = lu.queue.pop_front()
            {
                let full = with_count(lu.queue.len(), &text);
                lu.shown = Some((text, full.clone()));
                self.notice = Some(Notice { text: full, kind, action: None });
            }
        }
        self.live = Some(lu);
    }

    /// The sheet index and cell of the open cell editor when it is on the live workbook.
    fn live_editor_slot(&self, doc: u64) -> Option<(usize, gridcraft_engine::core::CellRef)> {
        let ed = self.editor.as_ref().filter(|e| e.doc == doc)?;
        let d = self.session.documents().iter().find(|d| d.uid == doc)?;
        Some((ed.sheet_in(&d.wb).unwrap_or(ed.sheet), ed.cell))
    }

    /// After a pump: the editor goes to where its row and column are now. When its row or column
    /// is gone, the editor closes and what was typed comes back, for the notice.
    fn follow_live_editor(&mut self, lu: &LiveUi) -> Option<Option<String>> {
        match lu.live.editor_at(&self.session) {
            Some((_, cell)) => {
                if let Some(ed) = self.editor.as_mut().filter(|e| e.doc == lu.doc)
                    && ed.cell != cell
                {
                    ed.cell = cell;
                    // Point mode held cells by position.
                    ed.point = None;
                    ed.point_cell = None;
                    ed.point_anchor = None;
                }
                None
            }
            None => {
                let ed = self.editor.take_if(|e| e.doc == lu.doc)?;
                self.session.mode = gridcraft_engine::Mode::Ready;
                let typed = ed.original.as_ref() != Some(&ed.text) && !ed.text.is_empty();
                Some(typed.then_some(ed.text))
            }
        }
    }

    /// Says the cell being edited was deleted, and shows the entry whole in a message box (so it
    /// is not lost, whatever its length) as well as in the notice queue.
    fn tell_editor_gone(&mut self, lu: &mut LiveUi, draft: Option<String>) {
        let lang = self.ui.language;
        let Some(d) = draft else {
            lu.push_notice(lang.tr(EDITOR_GONE_TEXT).to_owned(), NoticeKind::Warning);
            return;
        };
        let shown: String =
            if d.chars().count() > DRAFT_SHOWN { d.chars().take(DRAFT_SHOWN).chain(std::iter::once('…')).collect() } else { d.clone() };
        lu.push_notice(lang.tr_fill(EDITOR_GONE_DRAFT_TEXT, &[("draft", &shown)]), NoticeKind::Warning);
        let whole = lang.tr_fill(EDITOR_GONE_DRAFT_TEXT, &[("draft", &d)]);
        self.message = Some(match self.message.take() {
            Some((title, earlier)) => (title, format!("{earlier}\n\n{whole}")),
            None => ("GridCraft".into(), whole),
        });
    }

    /// Selects the cell another editor is on (a click on their badge), going to their sheet first.
    pub fn live_select_peer(&mut self, id: &str) {
        let Some(pos) = self.live.as_ref().and_then(|l| l.peers.iter().find(|p| p.id == id)).and_then(|p| p.caret.clone()) else { return };
        let on_sheet = self.session.active().is_some_and(|d| d.wb.active_sheet() == pos.sheet);
        if !on_sheet && let Err(e) = self.run("sheet.activate", json!({"sheet": pos.sheet})) {
            self.message = Some(("GridCraft".into(), crate::clean_error(&e)));
            return;
        }
        if let Err(e) = self.run("selection.set", json!({"cell": pos.active.a1()})) {
            self.message = Some(("GridCraft".into(), crate::clean_error(&e)));
        }
    }
}

/// The link's status, and who the window waits for, in the status bar.
pub(crate) fn status(app: &SheetApp, ui: &mut egui::Ui, t: &crate::theme::Tokens) {
    let Some(lu) = app.live() else { return };
    let lang = app.ui.language;
    let (text, bad) = lu.status_text(lang);
    ui.add_space(12.0);
    let colour = if bad { t.danger } else { t.text_dim };
    ui.add(egui::Label::new(egui::RichText::new(text).font(crate::theme::ui_font(12.5)).color(colour)).selectable(false));
    if let Some(line) = lu.status_line(lang) {
        ui.add_space(12.0);
        ui.add(egui::Label::new(egui::RichText::new(line).font(crate::theme::ui_font(12.5)).color(t.text_dim)).selectable(false));
    }
}

/// The presence bar, at the right of the status bar. A click on a badge selects that peer's cell.
pub(crate) fn presence_bar(app: &mut SheetApp, ui: &mut egui::Ui) {
    let Some(lu) = app.live.as_mut() else { return };
    if !lu.bar.shared() {
        return;
    }
    let clicked = lu.bar.show(ui);
    ui.add_space(8.0);
    if let Some(id) = clicked {
        app.live_select_peer(&id);
    }
}
