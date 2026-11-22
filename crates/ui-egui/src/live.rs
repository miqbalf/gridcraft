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

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
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
/// Longest draft the info bar shows; a longer one is also shown whole in a message box.
const DRAFT_SHOWN: usize = 160;

const BLOCKED_ID: &str = "gridcraft.live.blocked";

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
    blocked: Arc<BTreeSet<String>>,
}

impl LiveUi {
    /// Wraps a started session; `doc` is the workbook it edits.
    #[must_use]
    pub fn new(live: Live, doc: u64) -> LiveUi {
        let blocked = Arc::new(blocked_names(&live));
        LiveUi { live, doc, peers: Vec::new(), bar: PresenceBar::new(), moved: HashMap::new(), viewers: HashSet::new(), blocked }
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

    /// Takes in the roster after a pump: tracks who moved, and refreshes the presence bar.
    fn refresh(&mut self, mut peers: Vec<Peer>, now: f64, lang: Language, names: &dyn Fn(usize) -> String) {
        // A caret on an id this window does not know yet is not drawn until it is known.
        for p in &mut peers {
            p.caret = p.caret.take().filter(|c| !c.hidden);
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
/// command the live session refuses. False when the window is not live.
pub(crate) fn tip_blocked(ctx: &egui::Context, tip: &str) -> bool {
    let Some(set) = ctx.data(|d| d.get_temp::<Arc<BTreeSet<String>>>(egui::Id::NULL.with(BLOCKED_ID))) else { return false };
    set.contains(&tip.replace('\n', " "))
}

/// The tooltip of a greyed control.
pub(crate) fn blocked_tip(ctx: &egui::Context) -> String {
    crate::i18n::current(ctx).tr(NOT_YET_TEXT).to_owned()
}

impl SheetApp {
    /// The live session, if the window is in one.
    #[must_use]
    pub fn live(&self) -> Option<&LiveUi> {
        self.live.as_ref()
    }

    pub fn live_mut(&mut self) -> Option<&mut LiveUi> {
        self.live.as_mut()
    }

    /// Attaches a live session started on the active workbook, for a host that has it only after
    /// the window exists (the web host). The session must have been started on the session's
    /// active document, which `GridLive::start` leaves active.
    ///
    /// # Errors
    /// The window is already live, or has no workbook open.
    pub fn attach_live(&mut self, live: Live) -> Result<(), String> {
        if self.live.is_some() {
            return Err("This window is already editing live".into());
        }
        let doc = self.session.active().map(|d| d.uid).ok_or_else(|| "There is no workbook to edit live".to_owned())?;
        self.live = Some(LiveUi::new(live, doc));
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
    pub(crate) fn engine_run(&mut self, id: &str, params: Json) -> Result<Json, String> {
        match self.live.as_mut() {
            Some(lu) => match lu.live.run(&mut self.session, id, params)? {
                Ran::Done(done) => Ok(done),
                Ran::Queued => Ok(json!({"queued": true})),
            },
            None => self.session.run(id, params),
        }
    }

    /// One frame of a live window: anchors the open editor, takes in the room, moves the editor
    /// with its row, and shows what the room told.
    pub(crate) fn live_frame(&mut self, ctx: &egui::Context) {
        let Some(mut lu) = self.live.take() else { return };
        let now = ctx.input(|i| i.time);
        let lang = self.ui.language;
        let slot = self.live_editor_slot(lu.doc);
        lu.live.anchor_editor(slot);
        // Only an anchor that resolves can be lost: one that never took (a cell outside the
        // layout) must not close the editor.
        let tracked = slot.is_some() && lu.live.editor_at(&self.session).is_some();
        let peers = lu.live.pump(&mut self.session);
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
        self.live = Some(lu);
        if let Some(draft) = gone {
            self.tell_editor_gone(draft);
        }
        if let Some(text) = notes.last() {
            self.notice = Some(Notice { text: lang.tr(text).to_owned(), kind: NoticeKind::Info, action: None });
        }
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

    /// Says the cell being edited was deleted, and shows the entry so it is not lost.
    fn tell_editor_gone(&mut self, draft: Option<String>) {
        let lang = self.ui.language;
        let text = match &draft {
            None => lang.tr(EDITOR_GONE_TEXT).to_owned(),
            Some(d) => {
                let shown: String =
                    if d.chars().count() > DRAFT_SHOWN { d.chars().take(DRAFT_SHOWN).chain(std::iter::once('…')).collect() } else { d.clone() };
                lang.tr_fill(EDITOR_GONE_DRAFT_TEXT, &[("draft", &shown)])
            }
        };
        self.notice = Some(Notice { text, kind: NoticeKind::Warning, action: None });
        if let Some(d) = draft.filter(|d| d.chars().count() > DRAFT_SHOWN) {
            self.message = Some(("GridCraft".into(), lang.tr_fill(EDITOR_GONE_DRAFT_TEXT, &[("draft", &d)])));
        }
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
