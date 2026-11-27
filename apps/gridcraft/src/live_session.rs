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
//! - When the session can't start, the file opens on its own and the window says why; its
//!   changes then stay on this computer (the launcher uploads nothing for a live open).
//!
//! The ticket is never logged.

use std::path::Path;

use gridcraft_engine::Session;
use gridcraft_live::desktop::{Rewrite, SessionFile, SessionWatch};
use gridcraft_live::{BaseFile, GridLive, schema};
use gridcraft_ui_egui::live::{DynHub, Live};
use gridcraft_ui_egui::{Notice, NoticeKind, SheetApp};

/// Told when the session could not start; `{error}` says why.
pub const NOT_LIVE_TEXT: &str =
    "Couldn't open the file for live editing: {error}. This window edits the copy on this computer, and its changes are not uploaded.";

/// The joined session's host side: the session file's watch, and the runtime the link runs on.
pub struct DesktopLive {
    watch: SessionWatch,
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
    Ok((live, DesktopLive { watch: SessionWatch::new(session_file, &file), _rt: rt }))
}

impl DesktopLive {
    /// Once a frame: a fresh ticket from the launcher goes to the link; a rewrite for another
    /// room or with less access ends the session.
    pub fn frame(&mut self, app: &mut SheetApp, ctx: &egui::Context) {
        let now = ctx.input(|i| i.time);
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
        ctx.request_repaint_after(std::time::Duration::from_secs_f64(gridcraft_live::desktop::WATCH_SECS));
    }
}

/// Tells why the session did not start, in the window's language.
pub fn tell_not_live(app: &mut SheetApp, error: &str) {
    let lang = app.ui.language;
    let text = lang.tr_fill(NOT_LIVE_TEXT, &[("error", lang.tr(error))]);
    app.notice = Some(Notice { text, kind: NoticeKind::Warning, action: None });
}
