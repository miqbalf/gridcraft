//! Host modes: a page that runs GridCraft (as an iframe, or in its own document) opens workbooks
//! in it and stores what it saves instead of the browser downloading. The messages and the two
//! transports are `craft-web-host`'s (storytold/craft-libs), the same contract as the other Craft
//! apps' bridges, with the `gridcraft:` prefix. This module is GridCraft's part of it.
//!
//! - `?host=parent` (an `<iframe>`, such as a Nextcloud app) talks to the same-origin parent over
//!   `postMessage`; `data-host="page"` on the canvas (the page's own document, no iframe) uses
//!   DOM `CustomEvent`s on the canvas instead. `?host=parent` wins when both are present. With no
//!   parent, or a parent on another origin, there is no bridge: the normal app with downloads.
//! - `gridcraft:ready` once the app is listening; send a file now. GridCraft reads bytes, so it
//!   says `file: false` and a host sends `bytes`.
//! - `gridcraft:open` (from the host): `{ name, bytes }`. It opens like a workbook picked with
//!   File › Open or dropped on the window; a file over [`MAX_OPEN_BYTES`] is refused before it is
//!   copied.
//! - `gridcraft:save` (to the host): `{ name, saveAs, bytes, export }` for every file GridCraft
//!   would otherwise download. With `export: false`, the host stores the XLSX workbook; with
//!   `export: true`, it stores a separate file (such as a PDF) without replacing the workbook.
//!   `saveAs` is `false`, except for a host that answers saves (below).
//! - `gridcraft:host` with `saves: true` (from the host): the host stores saves and answers each.
//!   `gridcraft:save` then carries an `id`, and `saveAs` is `true` for a workbook under another
//!   name than the file the host opened. The workbook counts as saved only on
//!   `gridcraft:saved { id, name? }` (a `name` is its name from then on: a conflict copy), and on
//!   `gridcraft:save-failed { id, error }` it stays unsaved with "Couldn't save to Nextcloud:
//!   {error}". Without `saves` nothing changes: a file handed over counts as saved at once.
//! - `gridcraft:fonts` (from the host) is ignored: GridCraft has no user-font path on the web
//!   (no system fonts, and a font file cannot be bound to a family there), so nothing is lost by
//!   a host sending it.
//! - `gridcraft:dirty` whenever unsaved changes appear or go away, `gridcraft:failed` when the
//!   app couldn't start.
//!
//! # Live editing (`craft-live/1`, `?host=parent` only)
//!
//! The parent page may instead send the unprefixed `live` message: `{ticket, relayUrl, room,
//! fileUrl, fileName, proto}` (`craft-web-host`'s `live` module checks every field and refuses
//! another `proto`). GridCraft posts the unprefixed `{type:"ready"}` once it listens.
//!
//! - The first `live` fetches `fileUrl` (the room's base file) with the user's cookies, only from
//!   this page's own origin over HTTP(S) ([`crate::live_ticket::same_origin`]); a 404 is retried
//!   briefly (the file may still be written). The URL must carry the base's `sha256`, and bytes
//!   that hash differently are refused ([`crate::live_ticket::checked_base`]), so there is no join.
//! - It then connects to the relay on the page's own host only ([`crate::live_ticket::same_host`];
//!   a relative `/craft` is) over the page's `socket.io-client` (`craft-collab`'s `WebHub`),
//!   announcing the `gridcraft/1_<build>` schema, and starts `GridLive` on the file with the
//!   URL's hash and the ticket's access (a ticket without the update bit joins read-only).
//!   If the fetch, the connect or the start fails, the window says why and the next `live` tries
//!   again ([`crate::live_ticket::Joining`]).
//! - A later `live` for the same room is the page's ticket refresh: it goes to the hub
//!   (`Hub::set_token`). One for another room, or one that gives less access, ends the session.
//!   A message while the file is fetched waits for the join.
//! - Another `proto` shows "This file is being edited with a newer GridCraft. Update to join."
//! - `live-state` `{state, reason, savedAt, peers}` goes to the parent on every change of state
//!   and at most once a second otherwise.
//! - While live, the workbook is not saved from this window (the document service saves the
//!   room), so workbook saves are refused. Exports still go out. `gridcraft:dirty` is true only
//!   while edits wait for the relay's confirmation (unless the link broke: those became the copy
//!   below) or a copy waits to be stored.
//! - Local edits craft-live had to drop (taken back, a resync, damage, a replaced or refused
//!   replica) and edits the room never confirmed when the session ended come back as a copy of the
//!   workbook, `"{stem} (unmerged YYYY-MM-DD HHMM).xlsx"`. It goes to the page at once as a
//!   The info bar offers it (Save a copy) for as long as it waits; nothing is saved unasked.
//!   On Save a copy it goes to the page as a `gridcraft:save` with `saveAs: true` and an id
//!   `copy-N` (a host that does not answer saves gets the browser's plain download instead) and
//!   stays in the window until the page says `saved`; on `save-failed`, or no answer within 30
//!   seconds, Save a copy downloads it. A waiting copy keeps `gridcraft:dirty` true.
//!
//! Tickets are never logged.
//!
//! In both host modes the page starts with an empty workbook instead of a sample, saves and PDF
//! exports are handed to the page rather than downloaded, and `?author=` names the user for new
//! notes and comments.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use craft_collab::Hub as _;
use craft_live::LiveStatus;
use craft_web_host::live::{LiveMessage, LiveState, Peer as HostPeer};
use craft_web_host::web::{FileData, Host, Message};
use craft_web_host::{Mode, Outgoing, file_name, message_type};
use gridcraft_live::{BaseFile, LiveFail, schema};
use gridcraft_ui_egui::live::DynHub;
use gridcraft_ui_egui::{Inbox, Notice, NoticeKind, Services, SheetApp};
use wasm_bindgen::{JsCast as _, JsValue};

use crate::host_save::{Answer, Receipts, SaveMessage, sync_saved};
use crate::hostmode::open_name;
use crate::live_ticket::{Joining, Next};

/// The app id in the host messages (`gridcraft:ready`).
const APP: &str = "gridcraft";

/// The largest workbook taken from the host page, refused before it is copied.
const MAX_OPEN_BYTES: u64 = 512 << 20;

/// The longest user name taken from `?author=`.
const MAX_AUTHOR_CHARS: usize = 200;

#[derive(Clone)]
pub struct Bridge {
    host: Rc<Host>,
    save_target: Rc<SaveTarget>,
    /// Names of the files handed to the host since the last frame (a host that does not answer).
    saved: Rc<RefCell<Vec<String>>>,
    /// The saves a host that answers each is yet to answer.
    receipts: Rc<RefCell<Receipts>>,
    /// What the host answered, for the next frame.
    answers: Rc<RefCell<Vec<Answer>>>,
    /// What the host answered to copies of unmerged live edits (ids `copy-N`), for the next frame.
    copy_answers: Rc<RefCell<Vec<Answer>>>,
    /// The last copy id handed out.
    copies: Rc<Cell<u64>>,
    /// The name of the file the host sent: a save under another name is a new file.
    opened: Rc<RefCell<Option<String>>>,
    /// The window edits a room: its document service saves the workbook, this window does not.
    live_on: Rc<Cell<bool>>,
    /// The page sent a `live` message: this window is a room's, joined or not. Never cleared, so a
    /// failed join cannot turn into a save over the room's file.
    live_asked: Rc<Cell<bool>>,
}

impl Bridge {
    /// The bridge for this page, or `None` when GridCraft isn't hosted: no `?host=parent` and no
    /// `data-host="page"` on the canvas, an unframed `?host=parent` page (it stays the normal app
    /// with downloads), or a parent on another origin.
    pub fn detect(canvas: &web_sys::HtmlCanvasElement) -> Option<Self> {
        let host = Host::connect(APP, canvas)?;
        let save_target = match host.mode() {
            Mode::Parent => {
                // Host::connect already admitted only a same-origin parent. Keep that origin
                // as the postMessage target so later navigation can't receive workbook bytes.
                let window = web_sys::window()?;
                let parent = window.parent().ok().flatten()?;
                let origin = window.location().origin().ok()?;
                SaveTarget::Parent { parent, origin }
            }
            Mode::Page => SaveTarget::Page(canvas.clone()),
        };
        Some(Self {
            host: Rc::new(host),
            save_target: Rc::new(save_target),
            saved: Rc::default(),
            receipts: Rc::default(),
            answers: Rc::default(),
            copy_answers: Rc::default(),
            copies: Rc::default(),
            opened: Rc::default(),
            live_on: Rc::default(),
            live_asked: Rc::default(),
        })
    }

    /// Send saved and exported files to the host instead of downloading them, and hear files
    /// from it.
    pub fn connect(&self, services: &mut Services, inbox: Inbox, ctx: egui::Context) {
        self.listen(inbox, ctx);
        let bridge = self.clone();
        services.download = Some(Box::new(move |name: &str, bytes: &[u8]| {
            if let Err(e) = bridge.post_save(name, bytes) {
                log::error!("couldn't hand {name} to the page: {e}");
            }
        }));
        // The copy of unmerged live edits is a new file beside the room's, also while live.
        let bridge = self.clone();
        services.save_copy = Some(Box::new(move |name: &str, bytes: &[u8]| bridge.post_copy(name, bytes)));
    }

    /// `?author=`: the host's name for the user signs new notes and comments.
    pub fn set_author(app: &mut SheetApp, author: &str) {
        let author: String = author.trim().chars().take(MAX_AUTHOR_CHARS).collect();
        if !author.is_empty() {
            app.session.prefs.user_name = author;
        }
    }

    pub fn post_ready(&self) {
        let _ = self.host.post_ready(env!("CARGO_PKG_VERSION"), false);
        // A live host waits for the unprefixed `ready` before it sends `live`.
        if self.host.mode() == Mode::Parent {
            let _ = self.host.post_live_ready();
        }
    }

    pub fn post_failed(&self, error: &str) {
        let _ = self.host.post_failed(error, None);
    }

    /// After each frame: a workbook handed to the host this frame counts as saved under the
    /// name it was saved as, and the host hears when unsaved changes appear or go away (the
    /// crate does not repeat a state it already reported).
    pub fn sync(&self, app: &mut SheetApp) {
        let saved = std::mem::take(&mut *self.saved.borrow_mut());
        sync_saved(&saved, app);
        let answers = std::mem::take(&mut *self.answers.borrow_mut());
        {
            let mut receipts = self.receipts.borrow_mut();
            receipts.hold(app);
            for answer in answers {
                if let Some(name) = receipts.answer(app, answer) {
                    *self.opened.borrow_mut() = Some(name);
                }
            }
        }
        // The clock a copy handed to the page waits on (copies go only when the person asks).
        app.hand_over_copies(js_sys::Date::now() / 1000.0);
        for answer in std::mem::take(&mut *self.copy_answers.borrow_mut()) {
            let (id, answer) = match answer {
                Answer::Saved { id, name } => (id, Ok(name)),
                Answer::Failed { id, error } => (id, Err(error)),
            };
            app.copy_answered(&id, answer);
        }
        // A live room is saved by its document service: only edits the room has not confirmed
        // and copies of unmerged edits not stored yet are unsaved.
        let docs_dirty = app.session.documents().iter().any(|d| d.is_dirty());
        self.host.post_dirty(crate::live_ticket::page_dirty(self.live_on.get(), docs_dirty, app.live_unconfirmed(), app.copies_unsaved()));
    }

    /// The copy of unmerged live edits, when the person asked to save it (Save a copy): a new file
    /// (`saveAs: true`) with an id, so the window keeps the copy until the page says it stored it
    /// (or not). A host that does not answer saves has no `saveAs` in its contract, so the copy
    /// is the browser's plain download instead, which is the person's own (`Ok(None)`).
    fn post_copy(&self, path: &str, bytes: &[u8]) -> Result<Option<String>, String> {
        let (name, save_as) = crate::live_ticket::copy_save(path);
        if !self.receipts.borrow().confirms() {
            crate::web::download_plain(&name, bytes)?;
            return Ok(None);
        }
        self.copies.set(self.copies.get() + 1);
        let id = format!("copy-{}", self.copies.get());
        let mut message = SaveMessage::new(&name, bytes);
        message.save_as = save_as;
        message.id = Some(&id);
        self.save_target.post(message).map_err(|e| e.as_string().unwrap_or_else(|| format!("{e:?}")))?;
        Ok(Some(id))
    }

    fn post_save(&self, path: &str, bytes: &[u8]) -> Result<(), String> {
        let name = file_name(Some(path), "document");
        if let Some(why) = crate::live_ticket::save_refusal(self.live_on.get(), self.live_asked.get(), &name) {
            return Err(why.into());
        }
        let confirms = self.receipts.borrow().confirms();
        let id = confirms.then(|| self.receipts.borrow_mut().start(&name));
        let mut message = SaveMessage::new(&name, bytes);
        if let Some(id) = &id {
            message = message.answered(id, self.opened.borrow().as_deref());
        }
        if let Err(e) = self.save_target.post(message) {
            let error = e.as_string().unwrap_or_else(|| format!("{e:?}"));
            if let Some(id) = id {
                // No answer will come: say so now.
                self.receipts.borrow_mut().cancel(&id);
                self.answers.borrow_mut().push(Answer::Failed { id, error: error.clone() });
            }
            return Err(error);
        }
        if id.is_none() {
            self.saved.borrow_mut().push(name);
        }
        Ok(())
    }

    /// Where the answer to the save `id` goes: a copy of unmerged edits, or a workbook or export.
    fn answers_for(&self, id: &str) -> &Rc<RefCell<Vec<Answer>>> {
        if id.starts_with("copy-") { &self.copy_answers } else { &self.answers }
    }

    fn listen(&self, inbox: Inbox, ctx: egui::Context) {
        let bridge = self.clone();
        self.host.listen(move |message| match message {
            Message::Open { name, data } => {
                if deliver_open(&inbox, &ctx, &name, &data) {
                    *bridge.opened.borrow_mut() = Some(open_name(&name));
                }
            }
            Message::Host { saves: true, .. } => bridge.receipts.borrow_mut().set_confirms(),
            Message::Saved { id, name } => {
                bridge.answers_for(&id).borrow_mut().push(Answer::Saved { id, name });
                ctx.request_repaint();
            }
            Message::SaveFailed { id, error } => {
                bridge.answers_for(&id).borrow_mut().push(Answer::Failed { id, error });
                ctx.request_repaint();
            }
            _ => {}
        });
    }
}

/// A save-only adapter: the pinned host crate's `post_save` cannot add app-specific fields.
/// All other messages and transport detection still go through `Host`.
enum SaveTarget {
    Parent { parent: web_sys::Window, origin: String },
    Page(web_sys::HtmlCanvasElement),
}

impl SaveTarget {
    fn post(&self, save: SaveMessage<'_>) -> Result<(), JsValue> {
        let buffer = js_sys::Uint8Array::from(save.bytes).buffer();
        let kind = message_type(APP, Outgoing::Save.kind());
        let payload = js_sys::Object::new();
        let set = |key, value: JsValue| js_sys::Reflect::set(&payload, &JsValue::from_str(key), &value);
        set("name", save.name.into())?;
        set("bytes", buffer.clone().into())?;
        set("saveAs", JsValue::from_bool(save.save_as))?;
        set("export", JsValue::from_bool(save.export))?;
        if let Some(id) = save.id {
            set("id", id.into())?;
        }
        match self {
            Self::Parent { parent, origin } => {
                set("type", kind.as_str().into())?;
                parent.post_message_with_transfer(&payload, origin, &js_sys::Array::of1(&buffer))
            }
            Self::Page(canvas) => {
                let init = web_sys::CustomEventInit::new();
                init.set_detail(&payload.into());
                let event = web_sys::CustomEvent::new_with_event_init_dict(&kind, &init)?;
                canvas.dispatch_event(&event).map(|_| ())
            }
        }
    }
}

/// The file an open message carries, pushed onto the inbox for the next frame, like a dropped
/// file. A host's file is untrusted input: an oversized one is refused before it is copied into
/// the app's memory (the data's own length, not the host's claim).
fn deliver_open(inbox: &Inbox, ctx: &egui::Context, name: &str, data: &FileData) -> bool {
    let FileData::Bytes(array) = data else {
        log::warn!("host file {name} came as a File; GridCraft reads bytes only; not opened");
        return false;
    };
    if data.size() > MAX_OPEN_BYTES as f64 {
        log::warn!("host file is {} bytes, over the {} MiB host open limit; not opened", data.size(), MAX_OPEN_BYTES >> 20);
        return false;
    }
    inbox.lock().unwrap_or_else(|e| e.into_inner()).push((open_name(name), array.to_vec()));
    ctx.request_repaint();
    true
}

// ---- live editing ----

/// Told when the page speaks a newer live protocol than this build.
const NEWER_TEXT: &str = "This file is being edited with a newer GridCraft. Update to join.";

/// Told when the page's `live` message was refused.
const UNUSABLE_TEXT: &str = "Live editing could not start. Saving is off in this window; reopen the file.";

/// Longest wait for a base file that is not there yet, and how often it is asked for.
const BASE_RETRIES: u32 = 4;
const BASE_RETRY_MS: i32 = 500;
/// Told when the base file did not arrive in time; the local document stays usable.
const BASE_TIMEOUT: &str = "The file did not arrive within 30 seconds.";
/// The whole base fetch gives up after this.
const BASE_TIMEOUT_MS: i32 = 30_000;
/// The least time between two `live-state` posts that only differ in the peers or the save.
const REPORT_EVERY_MS: f64 = 1000.0;

impl Bridge {
    /// Hear the parent's `live` messages: the first fetches the file and joins, later ones are
    /// ticket refreshes. They wait in `events` for the next frame ([`Live::frame`]).
    pub fn listen_live(&self, events: LiveEvents, joining: Rc<RefCell<Joining>>, ctx: egui::Context) {
        let asked = self.live_asked.clone();
        self.host.listen_live(move |message| match message {
            Err(e) => {
                // The error never quotes the message, so no ticket reaches the log.
                log::warn!("live message refused: {e}");
                let event = match crate::live_ticket::note_refused(&asked, &e) {
                    crate::live_ticket::Refusal::Newer => LiveEvent::Newer,
                    crate::live_ticket::Refusal::Unusable => LiveEvent::Unusable,
                };
                events.borrow_mut().push(event);
                ctx.request_repaint();
            }
            Ok(message) => {
                asked.set(true);
                let access = crate::live_ticket::ticket_access(message.ticket());
                let next = joining.borrow_mut().message(&message.room, access);
                match next {
                    Next::Restarted | Next::AccessDropped => {
                        let reason = next.end_reason().unwrap_or(crate::live_ticket::RESTARTED);
                        log::warn!("the live session ended: {reason}");
                        events.borrow_mut().push(LiveEvent::End(reason));
                        ctx.request_repaint();
                    }
                    Next::Refresh => {
                        events.borrow_mut().push(LiveEvent::Ticket(message.ticket().to_owned()));
                        ctx.request_repaint();
                    }
                    Next::Open => {
                        let (events, ctx, joining) = (events.clone(), ctx.clone(), joining.clone());
                        wasm_bindgen_futures::spawn_local(async move {
                            let fetched = match fetch_file(&message.file_url).await {
                                Ok(bytes) => {
                                    crate::live_ticket::checked_base(&message.file_url, &bytes).map(|hash| (bytes, hash)).map_err(str::to_owned)
                                }
                                Err(e) => Err(e),
                            };
                            let event = match fetched {
                                Ok((bytes, hash)) => LiveEvent::Open(message, bytes, hash),
                                Err(e) => {
                                    // The page's next `live` (its refresh) tries again.
                                    joining.borrow_mut().failed();
                                    LiveEvent::Failed(e)
                                }
                            };
                            events.borrow_mut().push(event);
                            ctx.request_repaint();
                        });
                    }
                }
            }
        });
    }
}

/// What the parent's `live` messages brought, for the next frame.
pub enum LiveEvent {
    /// The base file, fetched and checked against its hash: join with this message's ticket.
    Open(LiveMessage, Vec<u8>, [u8; 32]),
    /// A later `live` for the room: a fresh ticket.
    Ticket(String),
    /// The file could not be fetched or checked.
    Failed(String),
    /// The room speaks a newer protocol.
    Newer,
    /// The page's `live` message could not be used; saving is off.
    Unusable,
    /// The session ended (another room, less access), for this reason.
    End(&'static str),
}

pub type LiveEvents = Rc<RefCell<Vec<LiveEvent>>>;

/// The web app's side of a live session: when it joins, a ticket that came during the join, and
/// what was last reported to the page.
#[derive(Default)]
pub struct Live {
    joining: Rc<RefCell<Joining>>,
    /// A fresh ticket that arrived while the file was fetched, for the hub once joined.
    pending: Option<String>,
    reported: Option<Reported>,
    /// The newest save marker seen, and when this window saw it (page milliseconds).
    saved: Option<(String, f64)>,
}

struct Reported {
    state: LiveState,
    at: f64,
}

impl Live {
    /// The join state the `live` listener shares ([`Bridge::listen_live`]).
    pub fn joining(&self) -> Rc<RefCell<Joining>> {
        Rc::clone(&self.joining)
    }

    /// Act on what arrived since the last frame, then tell the page how the session stands.
    pub fn frame(&mut self, app: &mut SheetApp, events: &LiveEvents, bridge: &Bridge) {
        let arrived = std::mem::take(&mut *events.borrow_mut());
        for event in arrived {
            match event {
                LiveEvent::Newer => tell(app, NEWER_TEXT),
                LiveEvent::Unusable => tell(app, UNUSABLE_TEXT),
                // Edits the room never confirmed come back as the copy (G16 review C1).
                LiveEvent::End(reason) => app.end_live_for(reason),
                LiveEvent::Failed(e) => {
                    log::error!("live file not fetched: {e}");
                    // A ticket that came during the fetch is older than the next join's.
                    self.pending = None;
                    not_opened(app, &e);
                }
                LiveEvent::Open(message, bytes, hash) => self.open(app, bridge, &message, &bytes, hash),
                // The hub uses it for its next connect, and starts again with it after the relay
                // refused an expired one.
                LiveEvent::Ticket(ticket) => {
                    if !refresh_ticket(app, &ticket) {
                        self.pending = Some(ticket);
                    }
                }
            }
        }
        self.report(app, bridge);
    }

    fn open(&mut self, app: &mut SheetApp, bridge: &Bridge, message: &LiveMessage, bytes: &[u8], hash: [u8; 32]) {
        let access = crate::live_ticket::ticket_access(message.ticket());
        let uid = crate::live_ticket::ticket_uid(message.ticket()).unwrap_or_else(|| "user".to_owned());
        let started = relay_on_page_host(&message.relay_url)
            .and_then(|()| {
                craft_collab::web::WebHub::connect_as(&message.relay_url, message.ticket(), None, Some(&schema())).map_err(|e| e.to_string())
            })
            .and_then(|hub| {
                let base = BaseFile { bytes, name: &message.file_name, expected_sha256: Some(hash) };
                gridcraft_live::GridLive::start(DynHub(Box::new(hub)), &message.room, &base, access, &uid, &mut app.session)
                    .map_err(|e| fail_text(&e))
            })
            .and_then(|mut live| {
                // The page's clock, for the caret limit and the lock's wait.
                let start = web_time::Instant::now();
                live.set_clock(Box::new(move || start.elapsed().as_secs_f64() * 1000.0));
                // The page's local date and time, for the name of a copy of unmerged edits.
                live.set_wall_clock(Box::new(local_now));
                app.attach_live(live)
            });
        match started {
            Ok(()) => {
                self.joining.borrow_mut().joined(&message.room, access);
                bridge.live_on.set(true);
                if let Some(ticket) = self.pending.take() {
                    refresh_ticket(app, &ticket);
                }
            }
            Err(e) => {
                // The page's next `live` (its refresh) tries again.
                self.joining.borrow_mut().failed();
                self.pending = None;
                log::error!("live editing did not start: {e}");
                not_opened(app, &e);
            }
        }
    }

    /// `live-state` to the page: at once when the state or its reason changes, otherwise at most
    /// once a second (peers, save marker).
    fn report(&mut self, app: &SheetApp, bridge: &Bridge) {
        let Some(ui) = app.live() else { return };
        let now = js_sys::Date::now();
        let status = ui.live().status();
        if let LiveStatus::Live { saved_at: Some(marker) } = status
            && self.saved.as_ref().is_none_or(|(seen, _)| seen != marker)
        {
            self.saved = Some((marker.clone(), now));
        }
        let (state, reason) = crate::live_ticket::host_status(status);
        let report = LiveState {
            state,
            reason,
            saved_at: self.saved.as_ref().and_then(|(_, ms)| js_sys::Date::new(&JsValue::from_f64(*ms)).to_iso_string().as_string()),
            peers: ui.peers().iter().map(|p| HostPeer { id: p.id.clone(), display: p.display.clone() }).collect(),
        };
        let due = match &self.reported {
            None => true,
            Some(last) if last.state == report => false,
            Some(last) => last.state.state != report.state || last.state.reason != report.reason || now - last.at >= REPORT_EVERY_MS,
        };
        if due {
            if let Err(e) = bridge.host.post_live_state(&report) {
                log::warn!("live-state not delivered: {e}");
            }
            self.reported = Some(Reported { state: report, at: now });
        }
    }
}

/// The page's local date and time (the wasm build has no system clock).
fn local_now() -> chrono::NaiveDateTime {
    let d = js_sys::Date::new_0();
    i32::try_from(d.get_full_year())
        .ok()
        .and_then(|year| chrono::NaiveDate::from_ymd_opt(year, d.get_month() + 1, d.get_date()))
        .and_then(|day| day.and_hms_opt(d.get_hours(), d.get_minutes(), d.get_seconds()))
        .unwrap_or_default()
}

/// A fresh ticket for the hub; false when the window is not live yet.
fn refresh_ticket(app: &mut SheetApp, ticket: &str) -> bool {
    match app.live_mut() {
        Some(live) => {
            live.live_mut().core_mut().hub_mut().set_token(ticket.to_owned());
            true
        }
        None => false,
    }
}

fn tell(app: &mut SheetApp, text: &str) {
    let text = app.ui.language.tr(text).to_owned();
    app.notice = Some(Notice { text, kind: NoticeKind::Warning, action: None });
}

fn not_opened(app: &mut SheetApp, error: &str) {
    let lang = app.ui.language;
    let text = lang.tr_fill("Couldn't open the file for live editing: {error}", &[("error", error)]);
    let text = format!("{text} {}", lang.tr(crate::live_ticket::SAVE_OFF_AFTER_FAILED_JOIN));
    app.notice = Some(Notice { text, kind: NoticeKind::Warning, action: None });
}

/// Why a session did not start, for the user.
fn fail_text(fail: &LiveFail) -> String {
    match fail {
        LiveFail::Open(e) => e.clone(),
        LiveFail::BaseMismatch => crate::live_ticket::BASE_CHANGED.to_owned(),
        LiveFail::Live(e) => e.to_string(),
    }
}

/// The relay URL must be on the page's own host (a relative `/craft` is), so the ticket goes
/// nowhere else.
fn relay_on_page_host(relay_url: &str) -> Result<(), String> {
    let window = web_sys::window().ok_or("no window")?;
    let location = window.location();
    let page = location.href().map_err(|_| "no page address")?;
    let page_host = location.host().map_err(|_| "no page host")?;
    let url = web_sys::Url::new_with_base(relay_url, &page).map_err(|_| "the relay address is not a URL")?;
    if crate::live_ticket::same_host(&url.protocol(), &url.host(), &page_host) { Ok(()) } else { Err("the relay is not on this site".into()) }
}

/// Waits `ms` milliseconds on the page's timer.
async fn sleep(ms: i32) {
    let promise = js_sys::Promise::new(&mut |resolve, _| {
        if let Some(window) = web_sys::window() {
            let _ = window.set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, ms);
        }
    });
    let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
}

/// The base file, fetched with the user's cookies from this page's own origin only. A 404 is
/// asked again a few times (the file may be on its way), nothing else is.
async fn fetch_file(file_url: &str) -> Result<Vec<u8>, String> {
    let window = web_sys::window().ok_or("no window")?;
    let location = window.location();
    let page_origin = location.origin().map_err(|_| "no page origin")?;
    let page = location.href().map_err(|_| "no page address")?;
    let url = web_sys::Url::new_with_base(file_url, &page).map_err(|_| "the file address is not a URL")?;
    if !crate::live_ticket::same_origin(&url.protocol(), &url.origin(), &page_origin) {
        return Err("the file is not on this site".into());
    }
    // One deadline for the whole fetch, retries and body included: aborting settles a hung fetch.
    let abort = web_sys::AbortController::new().map_err(|_| "the file could not be fetched")?;
    let signal = abort.signal();
    let timer = abort.clone();
    let on_timeout = wasm_bindgen::closure::Closure::once_into_js(move || timer.abort());
    let timeout = window.set_timeout_with_callback_and_timeout_and_arguments_0(on_timeout.unchecked_ref(), BASE_TIMEOUT_MS).ok();
    let fetched = fetch_bytes(&window, &url.href(), &signal).await;
    if let Some(id) = timeout {
        window.clear_timeout_with_handle(id);
    }
    match fetched {
        Err(_) if signal.aborted() => Err(BASE_TIMEOUT.to_owned()),
        other => other,
    }
}

async fn fetch_bytes(window: &web_sys::Window, href: &str, signal: &web_sys::AbortSignal) -> Result<Vec<u8>, String> {
    let mut tries = 0;
    let response = loop {
        let init = web_sys::RequestInit::new();
        init.set_signal(Some(signal));
        init.set_method("GET");
        init.set_mode(web_sys::RequestMode::SameOrigin);
        init.set_credentials(web_sys::RequestCredentials::SameOrigin);
        let response =
            wasm_bindgen_futures::JsFuture::from(window.fetch_with_str_and_init(href, &init)).await.map_err(|_| "the file could not be fetched")?;
        let response: web_sys::Response = response.dyn_into().map_err(|_| "the fetch gave no response")?;
        if response.status() == 404 && tries < BASE_RETRIES {
            tries += 1;
            sleep(BASE_RETRY_MS).await;
            continue;
        }
        break response;
    };
    if !response.ok() {
        return Err(format!("the server answered {}", response.status()));
    }
    let too_big = || format!("the file is over the {} MiB limit", MAX_OPEN_BYTES >> 20);
    if let Ok(Some(length)) = response.headers().get("content-length")
        && length.trim().parse::<u64>().is_ok_and(|n| n > MAX_OPEN_BYTES)
    {
        return Err(too_big());
    }
    let buffer = response.array_buffer().map_err(|_| "the file could not be read")?;
    let buffer = wasm_bindgen_futures::JsFuture::from(buffer).await.map_err(|_| "the file could not be read")?;
    let array = js_sys::Uint8Array::new(&buffer);
    if u64::from(array.length()) > MAX_OPEN_BYTES {
        return Err(too_big());
    }
    Ok(array.to_vec())
}
