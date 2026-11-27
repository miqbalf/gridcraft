//! `--live-session <file>`: the launcher hands this window a live co-editing session on a file in
//! Nextcloud (`gridcraft_live::desktop` describes the file). The window joins the same room as
//! the web GridCraft, through the same session ([`gridcraft_live::GridLive`]) over a native link
//! to the relay ([`gridcraft_live::native_link`]); the protocol is the web's.
//!
//! - The room opens on its pinned base, checked by hash; the window shows it with every change
//!   since the room opened.
//! - The launcher rewrites the file with a fresh ticket before the old one expires; the window
//!   looks every two seconds and hands it to the link, which switches over without a drop.
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

use gridcraft_engine::Session;
use gridcraft_live::desktop::{Rewrite, SessionFile, SessionWatch};
use gridcraft_live::{BaseFile, GridLive, schema};
use craft_live::LiveStatus;
use gridcraft_ui_egui::live::{DynHub, Live};
use gridcraft_ui_egui::{Notice, NoticeKind, SheetApp};

/// Told when the session could not start; `{error}` says why.
pub const NOT_LIVE_TEXT: &str =
    "Couldn't open the file for live editing: {error}. This window edits the copy on this computer, and its changes are not uploaded.";

/// How long the first join may take before the window opens its copy on its own.
pub const JOIN_GIVE_UP_SECS: f64 = 20.0;

/// Told when the relay was not reached in time.
const NOT_REACHED_TEXT: &str = "the live relay could not be reached";

/// The joined session's host side: the session file's watch, and the runtime the link runs on.
pub struct DesktopLive {
    watch: SessionWatch,
    session_file: PathBuf,
    working: PathBuf,
    /// When the first frame ran (egui seconds), and whether the session has been live since.
    first_frame: Option<f64>,
    went_live: bool,
    /// The window gave up on the join: the live workbook was closed, and the copy opens next frame
    /// with this reason.
    falling_back: Option<String>,
    done: bool,
    /// Whether "not being saved" was last told (the server's `saver`).
    told_not_saving: bool,
    /// Kept for the link's sockets; dropped with the window.
    _rt: tokio::runtime::Runtime,
}

/// Starts the session the file at `session_file` names on `session`, opening the room's base
/// (`working` is the launcher's copy of the file). The session's workbook is added to `session`.
///
/// # Errors
/// Why it did not start, for the user; nothing quotes the ticket.
pub fn start(session_file: &Path, working: &Path, session: &mut Session) -> Result<(Live, DesktopLive), String> {
    let file = SessionFile::load(session_file).map_err(|e| e.to_string())?;
    let relay = file.relay().map_err(|e| e.to_string())?;
    let (bytes, hash) = file.base_bytes(working).map_err(|e| e.to_string())?;
    let name = file.file_name(working);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .thread_name("gridcraft-live")
        .enable_all()
        .build()
        .map_err(|e| format!("the live link could not start: {e}"))?;
    let link = gridcraft_live::native_link::renewing_link(rt.handle(), &relay, &file.ticket, &schema());
    let uid = gridcraft_live::ticket::ticket_uid(&file.ticket).unwrap_or_else(|| "user".to_owned());
    let base = BaseFile { bytes: &bytes, name: &name, expected_sha256: Some(hash) };
    let mut live = GridLive::start(DynHub(Box::new(link)), &file.room, &base, file.access(), &uid, session).map_err(|e| e.to_string())?;
    // This computer's date and time, for the name of a copy of unmerged edits.
    live.set_wall_clock(Box::new(|| chrono::Local::now().naive_local()));
    log::info!("live: joining room {} for file {:?}", file.room, file.file_id);
    Ok((live, DesktopLive {
        watch: SessionWatch::new(session_file, &file),
        session_file: session_file.to_owned(),
        working: working.to_owned(),
        first_frame: None,
        went_live: false,
        falling_back: None,
        done: false,
        told_not_saving: false,
        _rt: rt,
    }))
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
    /// and closes the live workbook; the copy opens on its own at the next frame.
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
        if let Some(i) = app.session.document_index(doc) {
            app.session.set_active(i);
            if let Err(e) = app.run("file.close", serde_json::json!({"force": true, "document": true})) {
                log::warn!("live: the live workbook did not close: {e}");
            }
        }
        self.falling_back = Some(why);
    }

    /// Once a frame: a fresh ticket from the launcher goes to the link; a rewrite for another
    /// room or with less access ends the session.
    pub fn frame(&mut self, app: &mut SheetApp, ctx: &egui::Context) {
        if self.done {
            return;
        }
        let now = ctx.input(|i| i.time);
        if let Some(why) = self.falling_back.as_ref().filter(|_| app.live().is_none()) {
            app.open_path(&self.working.to_string_lossy());
            tell_not_live(app, why);
            self.done = true;
            return;
        }
        self.give_up(app, now);
        match self.watch.poll(now) {
            Some(Rewrite::Ticket(ticket)) => {
                if let Some(lu) = app.live_mut() {
                    lu.live_mut().renew_ticket(ticket);
                }
            }
            Some(Rewrite::End(reason)) => {
                log::warn!("live: the session ended: {reason}");
                app.end_live_for(reason);
            }
            None => {}
        }
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

/// Tells why the session did not start, in the window's language.
pub fn tell_not_live(app: &mut SheetApp, error: &str) {
    let lang = app.ui.language;
    let text = lang.tr_fill(NOT_LIVE_TEXT, &[("error", lang.tr(error))]);
    app.notice = Some(Notice { text, kind: NoticeKind::Warning, action: None });
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
