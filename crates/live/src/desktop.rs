//! The desktop app's side of a live session the launcher hands over: the session file
//! (`--live-session <file>`), the relay address it names, the base it must open, and the file's
//! rewrites with fresh tickets.
//!
//! The launcher writes `<data dir>/live/<random>.json` (mode 0600):
//!
//! ```json
//! {"v":1,"server":"https://cloud.example","fileId":123,"path":"/Docs/a.xlsx",
//!  "ticket":"…","relayUrl":"wss://cloud.example/craft","room":"…",
//!  "expiresAt":"2026-11-27T12:00:00Z","readOnly":false,"displayName":"…",
//!  "base":{"sha256":"<64 hex>","path":"/abs/base.xlsx"}}
//! ```
//!
//! - Unknown fields are ignored; an unknown `v` is refused, so the app opens the file on its own.
//! - `base` is the ticket's pinned base (GridCraft rooms open on a copy of the file the room pins,
//!   checked by hash). `base.sha256` is required; `base.path` is the downloaded base, and when it
//!   is missing the file argument (the launcher's working copy) must be those bytes.
//! - The launcher rewrites the file atomically with a fresh `ticket` before the old one expires.
//!   [`SessionWatch`] looks at its modification time every two seconds.
//!
//! The ticket is never logged: [`SessionFile`]'s `Debug` leaves it out, and no error quotes it.

use std::path::{Path, PathBuf};

use craft_live::Access;
use serde::Deserialize;
use sha2::{Digest, Sha256};

/// The session file versions this build reads.
pub const VERSION: u64 = 1;

/// How often the session file is looked at for a fresh ticket, in seconds.
pub const WATCH_SECS: f64 = 2.0;

/// Told when the session file names a version this build does not read.
pub const NEWER_SESSION_TEXT: &str = "The launcher speaks a newer live protocol than this GridCraft. Update GridCraft to edit live.";
/// Told when the session file carries no base checksum.
pub const NO_BASE_TEXT: &str = "The launcher did not say which file the live room was opened on.";
/// Told when the bytes on disk are not the room's base.
pub const BASE_CHANGED_TEXT: &str = "The file changed since the room was opened. Reopen the file.";
/// Told when the relay is not on the server's own host.
pub const RELAY_ELSEWHERE_TEXT: &str = "The live relay is not on the server's own address.";
/// Shown while the server says the room has no save worker (`saver: false`).
pub const NOT_SAVING_TEXT: &str = "Changes are not being saved right now";
/// The window's end when a rewrite names another room.
pub const RESTARTED_TEXT: &str = "The live session was restarted. Reopen the file.";
/// The window's end when a rewrite gives less access.
pub const ACCESS_CHANGED_TEXT: &str = "Your access changed. Reopen the file.";

/// The base the room was opened on, as the launcher passes it on.
#[derive(Clone, Default, Deserialize, PartialEq, Eq)]
pub struct SessionBase {
    /// The base's SHA-256, 64 hex digits.
    #[serde(default)]
    pub sha256: Option<String>,
    /// The downloaded base, when it is not the file argument.
    #[serde(default)]
    pub path: Option<PathBuf>,
}

/// A parsed session file.
#[derive(Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SessionFile {
    pub v: u64,
    pub server: String,
    #[serde(default)]
    pub file_id: Option<u64>,
    #[serde(default)]
    pub path: Option<String>,
    pub ticket: String,
    pub relay_url: String,
    pub room: String,
    #[serde(default)]
    pub expires_at: Option<String>,
    #[serde(default)]
    pub read_only: bool,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub base: Option<SessionBase>,
    /// The ticket answer's `saver`: `false` when the room has no save worker right now. Missing
    /// means saving.
    #[serde(default)]
    pub saver: Option<bool>,
}

impl std::fmt::Debug for SessionFile {
    // The ticket is left out, so a logged session file never carries it.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionFile")
            .field("v", &self.v)
            .field("server", &self.server)
            .field("file_id", &self.file_id)
            .field("room", &self.room)
            .field("expires_at", &self.expires_at)
            .field("read_only", &self.read_only)
            .finish_non_exhaustive()
    }
}

/// Why a session file could not be used. Its text never quotes the file.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SessionError {
    #[error("the live session file could not be read")]
    Unreadable,
    #[error("the live session file is not usable")]
    Malformed,
    #[error("{}", NEWER_SESSION_TEXT)]
    Newer,
    #[error("{}", RELAY_ELSEWHERE_TEXT)]
    RelayElsewhere,
    #[error("{}", NO_BASE_TEXT)]
    NoBase,
    #[error("{}", BASE_CHANGED_TEXT)]
    BaseChanged,
}

impl SessionFile {
    /// Parses a session file's bytes.
    ///
    /// # Errors
    /// [`SessionError::Malformed`] for bytes that are not a session file (fields missing or
    /// empty), [`SessionError::Newer`] for a version this build does not read.
    pub fn parse(bytes: &[u8]) -> Result<SessionFile, SessionError> {
        // The version first: a newer file may have changed any other field.
        let v = serde_json::from_slice::<serde_json::Value>(bytes)
            .ok()
            .and_then(|j| j.get("v").and_then(serde_json::Value::as_u64))
            .ok_or(SessionError::Malformed)?;
        if v != VERSION {
            return Err(SessionError::Newer);
        }
        let file: SessionFile = serde_json::from_slice(bytes).map_err(|_| SessionError::Malformed)?;
        if file.ticket.is_empty() || file.room.is_empty() || file.relay_url.is_empty() || file.server.is_empty() {
            return Err(SessionError::Malformed);
        }
        Ok(file)
    }

    /// Reads and parses the session file at `path`.
    ///
    /// # Errors
    /// [`SessionError::Unreadable`], or what [`SessionFile::parse`] refuses.
    pub fn load(path: &Path) -> Result<SessionFile, SessionError> {
        let bytes = std::fs::read(path).map_err(|_| SessionError::Unreadable)?;
        SessionFile::parse(&bytes)
    }

    /// The access to start with: read-only when the launcher says so or the ticket grants no
    /// edit.
    #[must_use]
    pub fn access(&self) -> Access {
        if self.read_only { Access::Read } else { crate::ticket::ticket_access(&self.ticket) }
    }

    /// Whether the room's edits are being saved: `false` only for an edit session the server
    /// says has no save worker (`saver: false`).
    #[must_use]
    pub fn saving(&self) -> bool {
        self.access() == Access::Read || self.saver != Some(false)
    }

    /// The relay to connect to, as `http(s)://host[:port]/path` for the native socket: a path
    /// (`/craft`) is on the server; `ws`/`wss` become `http`/`https`.
    ///
    /// # Errors
    /// [`SessionError::RelayElsewhere`] when it is on another host or port than the server, or
    /// not HTTP(S) or WebSocket, so the ticket goes nowhere else.
    pub fn relay(&self) -> Result<String, SessionError> {
        let (scheme, authority, _) = split_url(&self.server).ok_or(SessionError::RelayElsewhere)?;
        if !matches!(scheme.as_str(), "http" | "https") {
            return Err(SessionError::RelayElsewhere);
        }
        if self.relay_url.starts_with('/') {
            if self.relay_url.starts_with("//") || self.relay_url.contains('\\') {
                return Err(SessionError::RelayElsewhere);
            }
            return Ok(format!("{scheme}://{authority}{}", self.relay_url));
        }
        let (rs, ra, rest) = split_url(&self.relay_url).ok_or(SessionError::RelayElsewhere)?;
        let http = match rs.as_str() {
            "http" | "ws" => "http",
            "https" | "wss" => "https",
            _ => return Err(SessionError::RelayElsewhere),
        };
        if http != scheme || !ra.eq_ignore_ascii_case(&authority) {
            return Err(SessionError::RelayElsewhere);
        }
        Ok(format!("{http}://{authority}{rest}"))
    }

    /// The room's base hash, from `base.sha256`.
    ///
    /// # Errors
    /// [`SessionError::NoBase`] when there is none or it is not 64 hex digits.
    pub fn base_sha256(&self) -> Result<[u8; 32], SessionError> {
        let hex = self.base.as_ref().and_then(|b| b.sha256.as_deref()).ok_or(SessionError::NoBase)?;
        parse_sha256(hex).ok_or(SessionError::NoBase)
    }

    /// The base bytes to open the room on: `base.path` when the launcher gave one, else the file
    /// argument `working`, checked against `base.sha256`.
    ///
    /// # Errors
    /// [`SessionError::NoBase`] (no hash), [`SessionError::Unreadable`] (the file can't be read),
    /// [`SessionError::BaseChanged`] (the bytes are another file).
    pub fn base_bytes(&self, working: &Path) -> Result<(Vec<u8>, [u8; 32]), SessionError> {
        let want = self.base_sha256()?;
        let path = self.base.as_ref().and_then(|b| b.path.as_deref()).unwrap_or(working);
        let bytes = std::fs::read(path).map_err(|_| SessionError::Unreadable)?;
        let got: [u8; 32] = Sha256::digest(&bytes).into();
        if got == want { Ok((bytes, want)) } else { Err(SessionError::BaseChanged) }
    }

    /// The file's name for the window title and the format: the last part of `path`, else of the
    /// file argument.
    #[must_use]
    pub fn file_name(&self, working: &Path) -> String {
        self.path
            .as_deref()
            .and_then(|p| p.rsplit('/').next())
            .filter(|n| !n.is_empty())
            .map(str::to_owned)
            .or_else(|| working.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_else(|| "Book.xlsx".to_owned())
    }
}

/// `scheme`, `authority` (host and port, lower case) and the rest (path and query) of an
/// absolute URL; `None` for one with user info, no host, or a backslash or space.
fn split_url(url: &str) -> Option<(String, String, String)> {
    if url.chars().any(|c| c == '\\' || c.is_whitespace() || c.is_control()) {
        return None;
    }
    let (scheme, after) = url.split_once("://")?;
    let at = after.find(['/', '?', '#']).unwrap_or(after.len());
    let (authority, rest) = after.split_at(at);
    if authority.is_empty() || authority.contains('@') {
        return None;
    }
    let scheme = scheme.to_ascii_lowercase();
    let authority = authority.to_ascii_lowercase();
    // A default port is the same address as none.
    let authority = match (scheme.as_str(), authority.rsplit_once(':')) {
        ("http" | "ws", Some((host, "80"))) | ("https" | "wss", Some((host, "443"))) => host.to_owned(),
        _ => authority,
    };
    Some((scheme, authority, rest.split('#').next().unwrap_or_default().to_owned()))
}

/// 64 hex digits (either case) as bytes.
fn parse_sha256(hex: &str) -> Option<[u8; 32]> {
    if hex.len() != 64 || !hex.is_ascii() {
        return None;
    }
    let mut out = [0u8; 32];
    for (byte, pair) in out.iter_mut().zip(hex.as_bytes().chunks(2)) {
        let s = std::str::from_utf8(pair).ok()?;
        *byte = u8::from_str_radix(s, 16).ok()?;
    }
    Some(out)
}

/// What a rewrite of the session file means for the joined window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rewrite {
    /// A fresh ticket for the same room: hand it to the hub.
    Ticket(String),
    /// The session cannot go on (another room, less access): end it for this reason.
    End(&'static str),
}

/// Watches the session file for the launcher's rewrites.
pub struct SessionWatch {
    path: PathBuf,
    room: String,
    access: Access,
    ticket: String,
    saving: bool,
    /// The bytes of the last rewrite read whole, so each is told once. Compared by content, not
    /// by modification time: a rename can keep the time (coarse clocks, or two writes in one
    /// tick), and a rewrite between the session's start and the watch's would hide behind it.
    last: Option<Vec<u8>>,
    /// When it was last looked at, on the caller's clock (seconds).
    looked: Option<f64>,
}

impl SessionWatch {
    /// Watches `path`, which held `file` when the session started.
    #[must_use]
    pub fn new(path: &Path, file: &SessionFile) -> SessionWatch {
        SessionWatch {
            path: path.to_owned(),
            room: file.room.clone(),
            access: file.access(),
            ticket: file.ticket.clone(),
            saving: file.saving(),
            last: None,
            looked: None,
        }
    }

    /// Looks at the file when [`WATCH_SECS`] passed since the last look (`now` in seconds, any
    /// monotonic clock), and says what a rewrite means. A file that is gone, unreadable, half
    /// written or unchanged means nothing: the next look tries again.
    ///
    /// `now` must be a clock that runs while the window is hidden: a hidden egui window's input
    /// keeps the time of its last shown frame.
    pub fn poll(&mut self, now: f64) -> Option<Rewrite> {
        if self.looked.is_some_and(|at| now - at < WATCH_SECS) {
            return None;
        }
        self.looked = Some(now);
        self.look()
    }

    /// Reads the file now, whatever the cadence (after the relay refused the ticket, say), and
    /// says what a rewrite since the last read means.
    pub fn look(&mut self) -> Option<Rewrite> {
        let bytes = std::fs::read(&self.path).ok()?;
        if self.last.as_deref() == Some(bytes.as_slice()) {
            return None;
        }
        let file = SessionFile::parse(&bytes).ok()?;
        // Taken as seen only once it parsed: a rename caught half way is read again.
        self.last = Some(bytes);
        if file.room != self.room {
            return Some(Rewrite::End(RESTARTED_TEXT));
        }
        if self.access == Access::Edit && file.access() == Access::Read {
            return Some(Rewrite::End(ACCESS_CHANGED_TEXT));
        }
        self.saving = file.saving();
        if file.ticket == self.ticket {
            return None;
        }
        self.ticket.clone_from(&file.ticket);
        Some(Rewrite::Ticket(file.ticket))
    }
}

impl SessionWatch {
    /// The newest ticket read: the one the link should be using.
    #[must_use]
    pub fn ticket(&self) -> &str {
        &self.ticket
    }

    /// Whether the room's edits are saved, as of the last rewrite read ([`SessionFile::saving`]).
    #[must_use]
    pub fn saving(&self) -> bool {
        self.saving
    }
}

/// Where the app tells the launcher it opened the file on its own: `<session file>.fallback`.
#[must_use]
pub fn fallback_path(session_file: &Path) -> PathBuf {
    let mut p = session_file.as_os_str().to_owned();
    p.push(".fallback");
    PathBuf::from(p)
}

/// Tells the launcher this window could not go live and edits its copy on its own, so the
/// launcher switches to the check-out flow and uploads the saves: writes
/// `{"v":1,"reason":…}` to [`fallback_path`] (mode 0600 where there are modes), atomically.
/// `reason` is short English with no secrets (never the ticket).
///
/// # Errors
/// The file could not be written.
pub fn write_fallback(session_file: &Path, reason: &str) -> std::io::Result<()> {
    let path = fallback_path(session_file);
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let body = serde_json::json!({"v": VERSION, "reason": reason}).to_string();
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    {
        use std::io::Write as _;
        let mut f = options.open(&tmp)?;
        f.write_all(body.as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, &path)
}
