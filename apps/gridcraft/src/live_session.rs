//! `--live-session <file>`: the launcher hands this window a live co-editing session on a file in
//! Nextcloud (`gridcraft_live::desktop` describes the file). The window joins the same room as
//! the web GridCraft, through the same session ([`gridcraft_live::GridLive`]) over a native link
//! to the relay ([`gridcraft_live::native_link`]); the protocol is the web's.
//!
//! - The room opens on its pinned base, checked by hash; the window shows it with every change
//!   since the room opened.
//! - The launcher rewrites the file with a fresh ticket before the old one expires; the window
//!   looks every two seconds and hands it to the link, which switches over without a drop. The
//!   looks run on the app's own clock: a hidden or covered window runs only `App::logic`, whose
//!   egui time stays at the last shown frame (F5: the renewal was never read, and the relay
//!   dropped the link at the old ticket's expiry).
//! - When the relay refuses the ticket after the window was live, the window reads the file again
//!   at once and retries once with the newest ticket, saying "Reconnecting…"; if that is refused
//!   too it says it is not live, until the launcher hands a fresh ticket.
//! - Save on the live workbook is the room's (`Services::room_saves`): nothing is written or
//!   uploaded from here. Save As and the exports write local files.
//! - When the session can't start (an unusable session file, no or another base, a relay that
//!   refuses the ticket or is not reached within [`JOIN_GIVE_UP_SECS`]), the window writes
//!   `<session file>.fallback` first, then opens the launcher's copy on its own and says why: the
//!   launcher then checks the file out and uploads the saves. Once the window has been live it
//!   never falls back; a later drop reconnects.
//!
//! The ticket is never logged.

use std::path::{Path, PathBuf};

use craft_live::LiveStatus;
use gridcraft_engine::Session;
use gridcraft_live::desktop::{Rewrite, SessionFile, SessionWatch};
use gridcraft_live::{BaseFile, GridLive, schema};
use gridcraft_ui_egui::live::{DynHub, Live};
use gridcraft_ui_egui::{Notice, NoticeKind, SheetApp};

/// Told when the session could not start; `{error}` says why.
pub const NOT_LIVE_TEXT: &str =
    "Couldn't open the file for live editing: {error}. This window edits the copy on this computer, and its changes are not uploaded.";

/// How long the first join may take before the window opens its copy on its own.
pub const JOIN_GIVE_UP_SECS: f64 = 20.0;

/// Told when the relay was not reached in time.
const NOT_REACHED_TEXT: &str = "the live relay could not be reached";

/// The status while the window retries with the newest ticket after the relay refused one.
pub const RETRYING_TEXT: &str = "Reconnecting…";

/// The status once the retry was refused too.
pub const REFUSED_TEXT: &str = "Not live: the server did not accept this window's ticket. Changes made here are not shared.";

/// How long the retry may take to join before the window says it is not live: a native link
/// waits 10 s for the relay's answer.
pub const RETRY_GIVE_UP_SECS: f64 = 15.0;

/// After the relay refused the ticket of a window that was live.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Rejoin {
    Idle,
    /// Retried with the newest ticket, at this time (the app's clock).
    Retrying(f64),
    /// The retry was refused too: waiting for the launcher's next ticket.
    GaveUp,
}

/// The joined session's host side: the session file's watch, and the runtime the link runs on.
pub struct DesktopLive {
    watch: SessionWatch,
    session_file: PathBuf,
    working: PathBuf,
    /// When the first frame ran (the app's clock), and whether the session has been live since.
    first_frame: Option<f64>,
    went_live: bool,
    /// The window gave up on the join: the live workbook was closed, and the copy opens next frame
    /// with this reason.
    falling_back: Option<String>,
    done: bool,
    /// Whether "not being saved" was last told (the server's `saver`).
    told_not_saving: bool,
    rejoin: Rejoin,
    /// Seconds on a clock that runs while the window is hidden (not egui's input time).
    clock: Box<dyn FnMut() -> f64>,
    /// Kept for the link's sockets; dropped with the window.
    _rt: Option<tokio::runtime::Runtime>,
}

/// Seconds since the call, on the monotonic clock.
fn monotonic() -> Box<dyn FnMut() -> f64> {
    #[allow(clippy::disallowed_methods)] // the desktop app, never built for wasm
    let start = std::time::Instant::now();
    Box::new(move || start.elapsed().as_secs_f64())
}

/// Starts the session the file at `session_file` names on `session`, opening the room's base
/// (`working` is the launcher's copy of the file). The session's workbook is added to `session`.
///
/// # Errors
/// Why it did not start, for the user; nothing quotes the ticket.
pub fn start(session_file: &Path, working: &Path, session: &mut Session) -> Result<(Live, DesktopLive), String> {
    let file = SessionFile::load(session_file).map_err(|e| e.to_string())?;
    let relay = file.relay().map_err(|e| e.to_string())?;
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .thread_name("gridcraft-live")
        .enable_all()
        .build()
        .map_err(|e| format!("the live link could not start: {e}"))?;
    let link = gridcraft_live::native_link::renewing_link(rt.handle(), &relay, &file.ticket, &schema());
    start_on(session_file, &file, working, session, DynHub(Box::new(link)), Some(rt))
}

/// [`start`] on `hub`, the link to the room (`rt` runs it, when it needs a runtime).
fn start_on(
    session_file: &Path,
    file: &SessionFile,
    working: &Path,
    session: &mut Session,
    hub: DynHub,
    rt: Option<tokio::runtime::Runtime>,
) -> Result<(Live, DesktopLive), String> {
    let (bytes, hash) = file.base_bytes(working).map_err(|e| e.to_string())?;
    let name = file.file_name(working);
    let uid = gridcraft_live::ticket::ticket_uid(&file.ticket).unwrap_or_else(|| "user".to_owned());
    let base = BaseFile { bytes: &bytes, name: &name, expected_sha256: Some(hash) };
    let mut live = GridLive::start(hub, &file.room, &base, file.access(), &uid, session).map_err(|e| e.to_string())?;
    // This computer's date and time, for the name of a copy of unmerged edits.
    live.set_wall_clock(Box::new(|| chrono::Local::now().naive_local()));
    log::info!("live: joining room {} for file {:?}", file.room, file.file_id);
    Ok((
        live,
        DesktopLive {
            watch: SessionWatch::new(session_file, file),
            session_file: session_file.to_owned(),
            working: working.to_owned(),
            first_frame: None,
            went_live: false,
            falling_back: None,
            done: false,
            told_not_saving: false,
            rejoin: Rejoin::Idle,
            clock: monotonic(),
            _rt: rt,
        },
    ))
}

/// [`start`], and when it fails, the fallback signal for the launcher ([`fallback`]).
///
/// # Errors
/// Why the session did not start, for the user.
pub fn open(session_file: &Path, working: Option<&Path>, session: &mut Session) -> Result<(Live, DesktopLive), String> {
    let started = match working {
        Some(working) => start(session_file, working, session),
        None => Err("no file was named to open".to_owned()),
    };
    if let Err(why) = &started {
        fallback(session_file, why);
    }
    started
}

/// Tells the launcher this window edits its copy on its own (`<session file>.fallback`).
pub fn fallback(session_file: &Path, why: &str) {
    if let Err(e) = gridcraft_live::desktop::write_fallback(session_file, why) {
        log::error!("live: the fallback signal for the launcher was not written: {e}");
    }
}

impl DesktopLive {
    /// Gives up on a first join that was refused or did not happen in time: tells the launcher,
    /// opens the copy on its own and closes the live workbook; the next frame says why.
    fn give_up(&mut self, app: &mut SheetApp, now: f64) {
        if self.went_live || self.falling_back.is_some() {
            return;
        }
        let Some(lu) = app.live() else { return };
        let why = match lu.live().status() {
            LiveStatus::Live { .. } | LiveStatus::ReadOnly => {
                self.went_live = true;
                return;
            }
            LiveStatus::Refused(why) | LiveStatus::Broken(why) => why.clone(),
            LiveStatus::Connecting | LiveStatus::Reconnecting => {
                if now - *self.first_frame.get_or_insert(now) < JOIN_GIVE_UP_SECS {
                    return;
                }
                NOT_REACHED_TEXT.to_owned()
            }
        };
        log::warn!("live: not joined, opening the copy on its own: {why}");
        fallback(&self.session_file, &why);
        let doc = lu.live().doc();
        // The copy opens before the live workbook closes: a window closes with its last
        // workbook, and the app with its last window, so the copy would never show.
        app.open_path(&self.working.to_string_lossy());
        if let Some(i) = app.session.document_index(doc) {
            app.session.set_active(i);
            if let Err(e) = app.run("file.close", serde_json::json!({"force": true, "document": true})) {
                log::warn!("live: the live workbook did not close: {e}");
            }
        }
        if let Some(i) = app.session.documents().iter().position(|d| d.path.as_deref().map(Path::new) == Some(self.working.as_path())) {
            app.session.set_active(i);
        }
        self.falling_back = Some(why);
    }

    /// Once a frame: a fresh ticket from the launcher goes to the link; a rewrite for another
    /// room or with less access ends the session.
    pub fn frame(&mut self, app: &mut SheetApp, ctx: &egui::Context) {
        if self.done {
            return;
        }
        // Not `ctx.input(|i| i.time)`: that stops while the window is hidden or covered.
        let now = (self.clock)();
        // The copy is open already (`give_up`); said once the live session's own ending is shown.
        if let Some(why) = self.falling_back.as_ref().filter(|_| app.live().is_none()) {
            tell_not_live(app, why);
            self.done = true;
            return;
        }
        self.give_up(app, now);
        let rewrite = self.watch.poll(now);
        self.take(app, rewrite, now);
        self.rejoin_after_refusal(app, now);
        // The server says the room has no save worker: say so while it lasts (rule 15).
        let saving = self.watch.saving();
        if !saving && !self.told_not_saving {
            let text = app.ui.language.tr(gridcraft_live::desktop::NOT_SAVING_TEXT).to_owned();
            app.notice = Some(Notice { text, kind: NoticeKind::Warning, action: None });
        }
        self.told_not_saving = !saving;
        ctx.request_repaint_after(std::time::Duration::from_secs_f64(gridcraft_live::desktop::WATCH_SECS));
    }
}

impl DesktopLive {
    /// Acts on what the watch read: a fresh ticket goes to the link (a fresh try, too, after a
    /// refusal), an end ends the session.
    fn take(&mut self, app: &mut SheetApp, rewrite: Option<Rewrite>, now: f64) {
        match rewrite {
            Some(Rewrite::Ticket(ticket)) => {
                if let Some(lu) = app.live_mut() {
                    if self.went_live && matches!(lu.live().status(), LiveStatus::Refused(_)) {
                        self.retrying(lu, now);
                    }
                    lu.live_mut().renew_ticket(ticket);
                }
            }
            Some(Rewrite::End(reason)) => {
                log::warn!("live: the session ended: {reason}");
                app.end_live_for(reason);
            }
            None => {}
        }
    }

    fn retrying(&mut self, lu: &mut gridcraft_ui_egui::live::LiveUi, now: f64) {
        self.rejoin = Rejoin::Retrying(now);
        lu.set_link_note(Some(RETRYING_TEXT));
    }

    /// After the relay refused the ticket of a window that was live (an expired ticket, say):
    /// reads the session file at once and retries once with the newest ticket; when the retry
    /// does not join in [`RETRY_GIVE_UP_SECS`], says the window is not live. A join clears it.
    fn rejoin_after_refusal(&mut self, app: &mut SheetApp, now: f64) {
        if !self.went_live {
            return;
        }
        let Some(lu) = app.live_mut() else { return };
        if !matches!(lu.live().status(), LiveStatus::Refused(_)) {
            if self.rejoin != Rejoin::Idle {
                log::info!("live: joined again after the refusal");
                self.rejoin = Rejoin::Idle;
                lu.set_link_note(None);
            }
            return;
        }
        match self.rejoin {
            Rejoin::Idle => {
                log::warn!("live: the relay refused this window's ticket; reading the session file again and retrying once");
                let rewrite = self.watch.look();
                if matches!(rewrite, Some(Rewrite::End(_))) {
                    self.take(app, rewrite, now);
                    return;
                }
                // The newest ticket, fresh from the file or the one already handed.
                let ticket = match rewrite {
                    Some(Rewrite::Ticket(t)) => t,
                    _ => self.watch.ticket().to_owned(),
                };
                self.retrying(lu, now);
                lu.live_mut().renew_ticket(ticket);
            }
            Rejoin::Retrying(since) if now - since >= RETRY_GIVE_UP_SECS => {
                log::warn!("live: the retry was refused too; not live until the launcher hands a fresh ticket");
                self.rejoin = Rejoin::GaveUp;
                lu.set_link_note(Some(REFUSED_TEXT));
            }
            Rejoin::Retrying(_) | Rejoin::GaveUp => {}
        }
    }
}

/// Tells why the session did not start, in the window's language.
pub fn tell_not_live(app: &mut SheetApp, error: &str) {
    let lang = app.ui.language;
    let text = lang.tr_fill(NOT_LIVE_TEXT, &[("error", lang.tr(error))]);
    app.notice = Some(Notice { text, kind: NoticeKind::Warning, action: None });
}

#[cfg(test)]
mod tests {
    use super::*;
    use gridcraft_live::desktop::WATCH_SECS;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("gridcraft-live-session-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn session_file(d: &Path, sha256: Option<&str>) -> PathBuf {
        let mut j = serde_json::json!({"v": 1, "server": "https://cloud.example", "fileId": 3, "path": "/Docs/Plan.xlsx",
            "ticket": "e30.c2ln", "relayUrl": "/craft", "room": "oc:3:a", "readOnly": false});
        if let Some(sha) = sha256 {
            j["base"] = serde_json::json!({"url": "https://cloud.example/apps/craft_office/live/base/oc:3:a", "sha256": sha});
        }
        let path = d.join("s.json");
        std::fs::write(&path, j.to_string()).unwrap();
        path
    }

    fn reason(session_file: &Path) -> String {
        let signal = gridcraft_live::desktop::fallback_path(session_file);
        let j: serde_json::Value = serde_json::from_slice(&std::fs::read(signal).expect("the fallback signal is written")).unwrap();
        assert_eq!(j["v"], serde_json::json!(1));
        j["reason"].as_str().unwrap_or_default().to_owned()
    }

    /// A real workbook as the launcher's copy, and a session file for a relay nobody answers on.
    fn unreachable(d: &Path) -> (PathBuf, PathBuf, Vec<u8>) {
        let mut wb = gridcraft_engine::model::Workbook::new();
        if let Some(sh) = wb.sheet_mut(0) {
            sh.set_cell(
                gridcraft_engine::core::CellRef::new(0, 0),
                gridcraft_engine::model::Cell::value(gridcraft_engine::core::Value::Number(41.0)),
            );
        }
        let bytes = gridcraft_engine::io::save_bytes(&wb, "Plan.xlsx").unwrap();
        let working = d.join("Plan.xlsx");
        std::fs::write(&working, &bytes).unwrap();
        let sha: String = <sha2::Sha256 as sha2::Digest>::digest(&bytes).iter().map(|b| format!("{b:02x}")).collect();
        // Port 9 (discard) on the loopback: the connect is refused at once, and retried.
        let j = serde_json::json!({"v": 1, "server": "http://127.0.0.1:9", "fileId": 3, "path": "/Docs/Plan.xlsx",
            "ticket": "e30.c2ln", "relayUrl": "/craft", "room": "oc:3:a", "readOnly": false, "base": {"sha256": sha}});
        let path = d.join("s.json");
        std::fs::write(&path, j.to_string()).unwrap();
        (path, working, bytes)
    }

    /// One window frame at `t` seconds: the launcher's side, then the window's logic.
    fn frame(ctx: &egui::Context, app: &mut SheetApp, desk: &mut DesktopLive, t: f64) {
        frame_at(ctx, app, desk, t, t);
    }

    /// A frame whose egui time is `shown` (a hidden window's stays at its last shown frame) while
    /// the app's clock says `t`.
    fn frame_at(ctx: &egui::Context, app: &mut SheetApp, desk: &mut DesktopLive, shown: f64, t: f64) {
        desk.clock = Box::new(move || t);
        let mut out = ctx.run_ui(egui::RawInput { time: Some(shown), ..Default::default() }, |ui| {
            let ctx = ui.ctx().clone();
            desk.frame(app, &ctx);
            app.logic(&ctx);
        });
        // No renderer takes the frame's textures here.
        out.textures_delta.clear();
    }

    #[test]
    fn typing_waits_for_the_first_join_and_a_join_that_never_comes_opens_the_copy_intact() {
        let d = dir("unreached");
        let (path, working, bytes) = unreachable(&d);
        let mut session = Session::new();
        let (live, mut desk) = open(&path, Some(&working), &mut session).expect("the session starts, connecting");
        let services = gridcraft_ui_egui::Services { live: Some(live), room_saves: true, ..Default::default() };
        let mut app = SheetApp::new(session, services);
        let ctx = egui::Context::default();
        frame(&ctx, &mut app, &mut desk, 0.0);
        assert!(app.live().is_some_and(gridcraft_ui_egui::live::LiveUi::waiting_for_first_join), "connecting");
        app.begin_edit(Some("typed while connecting".into()), false);
        assert!(app.editor.is_none(), "no typing before the join");
        assert!(app.run("cell.set", serde_json::json!({"cell": "A2", "input": "lost?"})).is_err(), "no edit either");
        frame(&ctx, &mut app, &mut desk, 1.0);
        assert!(!gridcraft_live::desktop::fallback_path(&path).exists(), "not given up yet");
        // Past the join wait: the launcher is told, then the copy opens on its own.
        frame(&ctx, &mut app, &mut desk, JOIN_GIVE_UP_SECS + 1.0);
        // Never a frame without a workbook: the window closes with its last one, and the app with it.
        let shown = app.session.active().and_then(|d| d.path.clone());
        assert_eq!(shown.as_deref().map(Path::new), Some(working.as_path()), "the copy is open in the frame the live workbook closes");
        frame(&ctx, &mut app, &mut desk, JOIN_GIVE_UP_SECS + 2.0);
        frame(&ctx, &mut app, &mut desk, JOIN_GIVE_UP_SECS + 3.0);
        assert!(reason(&path).contains("could not be reached"), "{}", reason(&path));
        let told = app.notice.as_ref().map(|n| n.text.clone()).unwrap_or_default();
        assert!(told.contains("could not be reached"), "the window says why it isn't live: {told:?}");
        assert!(app.live().is_none(), "no longer live");
        let doc = app.session.active().expect("the copy is open");
        assert_eq!(doc.path.as_deref().map(Path::new), Some(working.as_path()), "the launcher's copy, saved where it lies");
        let a1 = doc.wb.sheet(0).and_then(|s| s.cell(gridcraft_engine::core::CellRef::new(0, 0))).map(|c| c.value.clone());
        assert_eq!(a1, Some(gridcraft_engine::core::Value::Number(41.0)), "with the file's content");
        assert_eq!(std::fs::read(&working).unwrap(), bytes, "and the copy on disk is intact");
        app.run("cell.set", serde_json::json!({"cell": "A2", "input": "7"})).expect("and editable now");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_copy_that_is_not_the_rooms_base_falls_back_and_tells_the_launcher() {
        let d = dir("mismatch");
        let working = d.join("Plan.xlsx");
        std::fs::write(&working, b"not the base").unwrap();
        let path = session_file(&d, Some(&"0".repeat(64)));
        let mut session = Session::new();
        let started = open(&path, Some(&working), &mut session);
        assert!(started.is_err(), "no live join on another file");
        assert!(session.documents().is_empty(), "nothing was opened as live");
        assert!(reason(&path).contains("changed"), "the launcher is told why");
        assert!(!reason(&path).contains("e30.c2ln"), "never the ticket");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_session_file_without_a_base_falls_back() {
        let d = dir("nobase");
        let working = d.join("Plan.xlsx");
        std::fs::write(&working, b"bytes").unwrap();
        let path = session_file(&d, None);
        assert!(open(&path, Some(&working), &mut Session::new()).is_err());
        assert!(!reason(&path).is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn an_unreadable_session_file_falls_back() {
        let d = dir("unreadable");
        let path = d.join("gone.json");
        assert!(open(&path, Some(&d.join("Plan.xlsx")), &mut Session::new()).is_err());
        assert!(!reason(&path).is_empty(), "the launcher still hears of it");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// The launcher's rewrite: a temporary file renamed over the session file, with `ticket`.
    fn rewrite(path: &Path, ticket: &str) {
        let mut j: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        j["ticket"] = serde_json::json!(ticket);
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, j.to_string()).unwrap();
        std::fs::rename(&tmp, path).unwrap();
    }

    fn status(app: &SheetApp) -> (String, bool) {
        app.live().expect("live").status_text(app.ui.language)
    }

    /// F5: a window covered by another runs only `App::logic`, and egui's time stays at its last
    /// shown frame, so a watch on egui's time never looked again and the launcher's renewal was
    /// missed until the relay dropped the link at the old ticket's expiry; the window still said
    /// "Live". Here the window is covered and busy (a frame every 16 ms) while the launcher
    /// renames fresh tickets over the session file, the relay refuses, and then takes it back.
    #[test]
    fn a_renewal_while_covered_is_handed_over_and_a_refusal_retries_once_with_the_newest_ticket() {
        use craft_live::testing::{MemRelay, ROOM};
        let d = dir("renewal");
        let (path, working, _) = unreachable(&d);
        let mut j: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        j["room"] = serde_json::json!(ROOM);
        std::fs::write(&path, j.to_string()).unwrap();
        rewrite(&path, "e30.first");
        let relay = MemRelay::new();
        let hub = relay.connect("alice", "Alice", true);
        let link = hub.clone();
        let file = SessionFile::load(&path).unwrap();
        let mut session = Session::new();
        let (live, mut desk) = start_on(&path, &file, &working, &mut session, DynHub(Box::new(hub)), None).expect("starts");
        let services = gridcraft_ui_egui::Services { live: Some(live), room_saves: true, ..Default::default() };
        let mut app = SheetApp::new(session, services);
        let ctx = egui::Context::default();
        let newest = |relay: &MemRelay| relay.tokens(&link).last().cloned();
        let mut t = 0.0;
        for _ in 0..5 {
            t += 0.1;
            frame(&ctx, &mut app, &mut desk, t);
        }
        assert_eq!(status(&app), ("Live".to_owned(), false), "joined while shown");
        // Covered from here on: egui's time stays where it was, the app's clock runs.
        let shown = t;
        let mut busy = |app: &mut SheetApp, desk: &mut DesktopLive, secs: f64, step: f64| {
            let end = t + secs;
            while t < end {
                t += step;
                frame_at(&ctx, app, desk, shown, t);
            }
        };
        rewrite(&path, "e30.second");
        busy(&mut app, &mut desk, WATCH_SECS + 0.5, 0.016);
        assert_eq!(newest(&relay).as_deref(), Some("e30.second"), "the renewal reached the link while covered");
        assert_eq!(status(&app).0, "Live");

        // Renewed again, and the relay refuses the link before the next look.
        rewrite(&path, "e30.third");
        relay.refuse(&link, "auth", "token rejected");
        busy(&mut app, &mut desk, 0.1, 0.016);
        assert_eq!(newest(&relay).as_deref(), Some("e30.third"), "read again at once, and retried with the newest ticket");
        assert_eq!(status(&app), (RETRYING_TEXT.to_owned(), true), "not \"Live\" while refused");
        relay.drop_link(&link);
        relay.restore_link(&link);
        busy(&mut app, &mut desk, 0.5, 0.016);
        assert_eq!(status(&app), ("Live".to_owned(), false), "live again once the relay took it");

        // Refused again, and the retry with the same newest ticket too: honestly not live.
        relay.refuse(&link, "auth", "token rejected");
        busy(&mut app, &mut desk, 0.1, 0.016);
        assert_eq!(status(&app).0, RETRYING_TEXT);
        let handed = relay.tokens(&link).len();
        assert_eq!(newest(&relay).as_deref(), Some("e30.third"));
        busy(&mut app, &mut desk, RETRY_GIVE_UP_SECS + 0.5, 0.05);
        assert_eq!(status(&app), (REFUSED_TEXT.to_owned(), true));
        assert_eq!(relay.tokens(&link).len(), handed, "retried once only");
        assert!(app.run("cell.set", serde_json::json!({"cell": "A2", "input": "7"})).is_err(), "and takes no edits");

        // The launcher's next ticket is a fresh try, and it joins.
        rewrite(&path, "e30.fourth");
        busy(&mut app, &mut desk, WATCH_SECS + 0.5, 0.016);
        assert_eq!(newest(&relay).as_deref(), Some("e30.fourth"));
        assert_eq!(status(&app).0, RETRYING_TEXT);
        relay.drop_link(&link);
        relay.restore_link(&link);
        busy(&mut app, &mut desk, 0.5, 0.016);
        assert_eq!(status(&app), ("Live".to_owned(), false));
        app.run("cell.set", serde_json::json!({"cell": "A2", "input": "7"})).expect("edits go to the room again");
        let _ = std::fs::remove_dir_all(&d);
    }
}
