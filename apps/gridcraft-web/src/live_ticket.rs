//! What the web app reads off a live ticket, a file URL and a relay URL before it joins a room,
//! and when it joins. Pure, so it is tested natively; `host` (wasm32) does the browser part.
//!
//! This is a copy of WordCraft's `live_ticket` (plus the base hash of the file URL), kept in step
//! by hand. Logged as debt for craft-libs (Ruling R21): one shared module there replaces both.

use craft_live::{Access, LiveStatus};
use craft_web_host::live::{LiveError, LiveStatus as HostStatus};
use sha2::{Digest, Sha256};

/// The window's end when the page sends a `live` message for another room after the join.
pub const RESTARTED: &str = "The live session was restarted. Reopen the file.";
/// The window's end when the page's refreshed ticket for the room gives less access.
pub const ACCESS_CHANGED: &str = "Your access changed. Reopen the file.";
/// Told when the file URL carries no `sha256`: the base cannot be checked, so there is no join.
pub const NO_BASE_HASH: &str = "The file address carries no checksum, so the file cannot be checked.";
/// Told when the fetched file is not the one the room was opened on.
pub const BASE_CHANGED: &str = "The file changed since the room was opened. Reopen the file.";

/// Added to the notice of a live open that failed.
pub const SAVE_OFF_AFTER_FAILED_JOIN: &str = "Saving is off in this window; reopen the file to edit it live.";
/// Why a workbook save is refused while the window belongs to a room.
const SAVE_BY_SERVICE: &str = "the document service saves this file while it is edited live";

/// The ticket's access and user, read the same way as the desktop app reads them.
pub use gridcraft_live::ticket::{ticket_access, ticket_uid};

/// Whether a resolved file URL may be fetched with the user's cookies: plain HTTP(S) on the
/// page's own origin only, so a forged `fileUrl` cannot point the session's credentials anywhere
/// else.
pub fn same_origin(protocol: &str, origin: &str, page_origin: &str) -> bool {
    matches!(protocol, "http:" | "https:") && !origin.is_empty() && origin == page_origin
}

/// Whether a resolved relay URL may be connected to with the user's ticket: HTTP(S) or
/// WebSocket on the page's own host only (a relative `/craft` resolves to it), so a forged
/// `relayUrl` cannot send the ticket anywhere else.
pub fn same_host(protocol: &str, host: &str, page_host: &str) -> bool {
    matches!(protocol, "http:" | "https:" | "ws:" | "wss:") && !host.is_empty() && host.eq_ignore_ascii_case(page_host)
}

/// The hash the room says its base file has: the `sha256` query parameter of the file URL, 64 hex
/// digits. `None` when there is none or it is not a SHA-256.
pub fn base_sha256(file_url: &str) -> Option<[u8; 32]> {
    let query = file_url.split('#').next()?.split_once('?')?.1;
    let hex = query.split('&').find_map(|pair| pair.strip_prefix("sha256="))?;
    if hex.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (byte, pair) in out.iter_mut().zip(hex.as_bytes().chunks(2)) {
        let digit = |c: u8| (c as char).to_digit(16);
        *byte = u8::try_from(digit(pair[0])? * 16 + digit(*pair.get(1)?)?).ok()?;
    }
    Some(out)
}

/// Checks the fetched base against the file URL's `sha256`: the hash to start the session with.
///
/// # Errors
/// The message to show, when the URL has no hash ([`NO_BASE_HASH`]) or the bytes differ from it
/// ([`BASE_CHANGED`]). There is no join then.
pub fn checked_base(file_url: &str, bytes: &[u8]) -> Result<[u8; 32], &'static str> {
    let expected = base_sha256(file_url).ok_or(NO_BASE_HASH)?;
    let actual: [u8; 32] = Sha256::digest(bytes).into();
    if actual == expected { Ok(expected) } else { Err(BASE_CHANGED) }
}

/// When the window joins a room, as the parent's `live` messages arrive: the first one fetches
/// the file and joins; later ones for the joined room are fresh tickets. A fetch or a join that
/// failed lets the next message try again.
#[derive(Debug, Default, PartialEq, Eq)]
pub enum Joining {
    #[default]
    Idle,
    /// Fetching the file, then joining.
    Opening,
    /// Joined this room, with this access.
    Joined(String, Access),
}

/// What to do with a `live` message.
#[derive(Debug, PartialEq, Eq)]
pub enum Next {
    /// Fetch the file and join.
    Open,
    /// Hand the ticket to the hub (or keep it for the join in progress).
    Refresh,
    /// A message for another room while still joined to one: the session was restarted (the
    /// room was closed and opened again), so the link ends ([`RESTARTED`]).
    Restarted,
    /// A ticket for the room that no longer lets this window edit: the link ends
    /// ([`ACCESS_CHANGED`]).
    AccessDropped,
}

impl Next {
    /// Why the link ends, for a message that ends it.
    pub fn end_reason(&self) -> Option<&'static str> {
        match self {
            Next::Restarted => Some(RESTARTED),
            Next::AccessDropped => Some(ACCESS_CHANGED),
            Next::Open | Next::Refresh => None,
        }
    }
}

impl Joining {
    /// A `live` message for `room`, with a ticket granting `access`, arrived.
    pub fn message(&mut self, room: &str, access: Access) -> Next {
        match self {
            Joining::Idle => {
                *self = Joining::Opening;
                Next::Open
            }
            Joining::Opening => Next::Refresh,
            Joining::Joined(joined, _) if joined != room => Next::Restarted,
            Joining::Joined(_, Access::Edit) if access == Access::Read => Next::AccessDropped,
            Joining::Joined(..) => Next::Refresh,
        }
    }

    /// The fetch or the join failed: the next message opens again.
    pub fn failed(&mut self) {
        *self = Joining::Idle;
    }

    /// The window joined `room` with `access`.
    pub fn joined(&mut self, room: &str, access: Access) {
        *self = Joining::Joined(room.to_owned(), access);
    }
}

/// Why a save of `name` must not go to the host, or `None` when it may. A window the page made a
/// room's (`asked`) never saves a workbook itself: joined, the document service saves the room;
/// not joined (the fetch, the check or the join failed), a save would replace the room's file with
/// a document that never was it. Exports are copies and always go.
pub fn save_refusal(joined: bool, asked: bool, name: &str) -> Option<&'static str> {
    if !(joined || asked) || !crate::host_save::is_workbook(name) {
        return None;
    }
    Some(if joined { SAVE_BY_SERVICE } else { SAVE_OFF_AFTER_FAILED_JOIN })
}

/// The `save` message that hands the copy of unmerged live edits to the page: its bare file name,
/// and always `saveAs: true` (a new file beside the room's, never over it).
#[must_use]
pub fn copy_save(path: &str) -> (String, bool) {
    let name = path.rsplit(['/', '\\']).next().filter(|n| !n.is_empty()).unwrap_or(path);
    (name.to_owned(), true)
}

/// Whether the page should warn before the tab closes. While live the document service saves the
/// workbook, so only edits the relay has not confirmed (reconnecting, or refused for now) and a
/// copy of unmerged edits not stored yet count; otherwise any unsaved workbook does.
#[must_use]
pub fn page_dirty(live: bool, docs_dirty: bool, unconfirmed: bool, copies_unsaved: bool) -> bool {
    copies_unsaved || if live { unconfirmed } else { docs_dirty }
}

/// What a `live` message the host crate refused means for the window.
#[derive(Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The room speaks a newer protocol.
    Newer,
    /// The message is unusable (a bad ticket, a relay that is not `ws(s)://`...).
    Unusable,
}

/// A `live` message was refused: the page meant this window to be a room's, so it is marked
/// (`asked`) and no workbook save goes to the host from now on ([`save_refusal`]), whatever the
/// reason. The error never quotes the message, so no ticket is shown or logged.
pub fn note_refused(asked: &std::cell::Cell<bool>, error: &LiveError) -> Refusal {
    asked.set(true);
    if matches!(error, LiveError::UnsupportedProto(_)) { Refusal::Newer } else { Refusal::Unusable }
}

/// The `live-state` state and reason for the session's status.
pub fn host_status(status: &LiveStatus) -> (HostStatus, Option<String>) {
    match status {
        LiveStatus::Connecting => (HostStatus::Connecting, None),
        LiveStatus::Live { .. } => (HostStatus::Live, None),
        LiveStatus::ReadOnly => (HostStatus::Readonly, None),
        LiveStatus::Reconnecting => (HostStatus::Reconnecting, None),
        LiveStatus::Refused(why) => (HostStatus::Refused, Some(why.clone())),
        // The page's contract has no state for a link that ended for good: it is refused.
        LiveStatus::Broken(why) => (HostStatus::Refused, Some(why.clone())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;

    fn ticket(claims: &str) -> String {
        format!("{}.c2lnbmF0dXJl", URL_SAFE_NO_PAD.encode(claims))
    }

    fn hex(hash: [u8; 32]) -> String {
        hash.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn the_unmerged_copy_is_saved_as_a_new_file_by_its_bare_name() {
        assert_eq!(copy_save("dir/book (unmerged 2026-11-24 1404).xlsx"), ("book (unmerged 2026-11-24 1404).xlsx".to_owned(), true));
        assert_eq!(copy_save("book.xlsx"), ("book.xlsx".to_owned(), true));
    }

    #[test]
    fn unconfirmed_edits_and_unsaved_copies_keep_the_page_dirty() {
        assert!(!page_dirty(true, true, false, false), "the service saves a live room");
        assert!(page_dirty(true, false, true, false), "edits the relay has not confirmed");
        assert!(page_dirty(true, false, false, true), "a copy of unmerged edits waits");
        assert!(page_dirty(false, false, false, true), "a copy waits after the link ended");
        assert!(page_dirty(false, true, false, false));
        assert!(!page_dirty(false, false, true, false), "not live: the workbooks decide");
    }

    #[test]
    fn ticket_access_reads_perm() {
        let read = ticket(r#"{"rm":"oc:156","uid":"bob","dsp":"bob","perm":1,"exp":1795198977,"pid":"bob~bd68c9c8"}"#);
        let edit = ticket(r#"{"rm":"oc:155","uid":"bob","dsp":"bob","perm":3,"exp":1795198977,"pid":"bob~7260a2f2"}"#);
        assert_eq!(ticket_access(&read), Access::Read);
        assert_eq!(ticket_access(&edit), Access::Edit);
        assert_eq!(ticket_access("not a ticket"), Access::Edit, "the relay decides what it can't be read for");
        assert_eq!(ticket_uid(&edit).as_deref(), Some("bob"));
        assert_eq!(ticket_uid("not a ticket"), None);
    }

    #[test]
    fn file_url_must_be_same_origin() {
        let page = "https://cloud.example";
        assert!(same_origin("https:", "https://cloud.example", page));
        assert!(!same_origin("https:", "https://evil.example", page));
        assert!(!same_origin("http:", "http://cloud.example", page), "another scheme is another origin");
        assert!(!same_origin("data:", "null", "null"), "no data or blob URLs");
    }

    #[test]
    fn only_the_pages_own_host_gets_the_ticket() {
        let page = "cloud.example";
        assert!(same_host("wss:", "cloud.example", page));
        assert!(same_host("https:", "Cloud.Example", page), "hosts compare without case");
        assert!(!same_host("wss:", "relay.evil.example", page));
        assert!(!same_host("wss:", "cloud.example:8443", page), "another port is another host");
        assert!(!same_host("javascript:", "", page));
    }

    #[test]
    fn base_hash_is_read_from_the_url_and_checked() {
        let bytes = b"the base workbook";
        let hash: [u8; 32] = Sha256::digest(bytes).into();
        let url = format!("/craft/base/155?x=1&sha256={}&y=2#frag", hex(hash));
        assert_eq!(base_sha256(&url), Some(hash));
        assert_eq!(checked_base(&url, bytes), Ok(hash));
        assert_eq!(checked_base(&url, b"other bytes"), Err(BASE_CHANGED), "wrong bytes are refused with a message");
        assert_eq!(checked_base("/craft/base/155", bytes), Err(NO_BASE_HASH), "a URL without a hash is refused");
        assert_eq!(base_sha256("/f?sha256=abcd"), None, "not 64 digits");
        assert_eq!(base_sha256(&format!("/f?sha256={}", "zz".repeat(32))), None, "not hex");
        assert_eq!(base_sha256(&format!("/f?sha256={}", hex(hash).to_uppercase())), Some(hash), "hex digits in either case");
    }

    #[test]
    fn a_failed_join_never_saves_over_the_rooms_file() {
        // The page sent `live`, the join failed: joined is false but the window is the room's.
        assert!(save_refusal(false, true, "Budget.xlsx").is_some(), "no workbook save after a failed join");
        assert!(save_refusal(true, true, "Budget.xlsx").is_some(), "the service saves a joined room");
        assert!(save_refusal(false, true, "Budget.pdf").is_none(), "an export is a copy");
        assert!(save_refusal(false, false, "Budget.xlsx").is_none(), "a window that was never asked to go live saves as before");
    }

    #[test]
    fn a_refused_live_message_turns_saving_off() {
        let asked = std::cell::Cell::new(false);
        let bad = craft_web_host::live::LiveMessage::parse(r#"{"type":"live","ticket":"t","relayUrl":"http://not-ws","room":"r"}"#);
        let Err(error) = bad else { panic!("the message should be refused") };
        assert!(save_refusal(false, asked.get(), "Budget.xlsx").is_none(), "before: a plain window saves");
        assert_eq!(note_refused(&asked, &error), Refusal::Unusable);
        assert!(save_refusal(false, asked.get(), "Budget.xlsx").is_some(), "after the refusal no workbook save goes out");
        assert!(!error.to_string().contains("not-ws"), "the error does not quote the message");
    }

    #[test]
    fn a_failed_start_is_tried_again() {
        let mut j = Joining::default();
        assert_eq!(j.message("oc:1", Access::Edit), Next::Open);
        assert_eq!(j.message("oc:1", Access::Edit), Next::Refresh, "a ticket while the file is fetched");
        j.failed();
        assert_eq!(j.message("oc:1", Access::Edit), Next::Open, "the next message opens again");
    }

    #[test]
    fn a_live_for_another_room_ends_the_session() {
        // The worker closed the room while this window was away; the page reopened the file
        // and now sends the new room's ticket.
        let mut j = Joining::default();
        assert_eq!(j.message("oc:155:aaa", Access::Edit), Next::Open);
        j.joined("oc:155:aaa", Access::Edit);
        let next = j.message("oc:155:bbb", Access::Edit);
        assert_eq!(next, Next::Restarted);
        assert_eq!(next.end_reason(), Some(RESTARTED));
    }

    #[test]
    fn a_refresh_for_the_same_room_only_replaces_the_token() {
        let mut j = Joining::default();
        j.message("oc:1", Access::Edit);
        j.joined("oc:1", Access::Edit);
        let next = j.message("oc:1", Access::Edit);
        assert_eq!(next, Next::Refresh);
        assert_eq!(next.end_reason(), None, "nothing ends");
        assert_eq!(j, Joining::Joined("oc:1".into(), Access::Edit), "still joined to the same room");
        let less = j.message("oc:1", Access::Read);
        assert_eq!(less, Next::AccessDropped, "edit to read (perm 3 to 1) ends the link");
        assert_eq!(less.end_reason(), Some(ACCESS_CHANGED));
        let mut r = Joining::default();
        r.message("oc:1", Access::Read);
        r.joined("oc:1", Access::Read);
        assert_eq!(r.message("oc:1", Access::Read), Next::Refresh, "a read ticket refreshed stays");
    }

    #[test]
    fn a_broken_link_is_reported_as_refused_with_its_reason() {
        assert_eq!(host_status(&LiveStatus::Broken("x".into())), (HostStatus::Refused, Some("x".into())));
        assert_eq!(host_status(&LiveStatus::Live { saved_at: None }), (HostStatus::Live, None));
        assert_eq!(host_status(&LiveStatus::ReadOnly).0, HostStatus::Readonly);
    }
}
