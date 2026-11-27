//! What a host reads off a live ticket before it joins: the access it grants and the user it
//! names. The web app (from the page's `live` message) and the desktop app (from the launcher's
//! session file) read the same ticket, so they read it here.
//!
//! A ticket is `base64url(claims).base64url(signature)`. The relay checks the signature and
//! enforces the permission; this only reads what the interface needs, and never logs it.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use craft_live::Access;

/// The relay's permission bit for changing the file (OCS numbering: read 1, update 2).
const PERMISSION_UPDATE: u64 = 2;

/// The claims of a ticket, or `None` when it can't be read.
fn claims(ticket: &str) -> Option<serde_json::Value> {
    let payload = ticket.split('.').next()?;
    let bytes = URL_SAFE_NO_PAD.decode(payload).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// The access a ticket grants, read from its `perm` claim. Only the interface uses it, to start a
/// read-only ticket read-only. A ticket that can't be read is taken as an edit ticket and left to
/// the relay: one it can't verify either is refused at the join (the window says why), and one
/// without the update bit has its first edit refused.
#[must_use]
pub fn ticket_access(ticket: &str) -> Access {
    let perm = claims(ticket).and_then(|c| c.get("perm").and_then(serde_json::Value::as_u64));
    match perm {
        Some(perm) if perm & PERMISSION_UPDATE == 0 => Access::Read,
        _ => Access::Edit,
    }
}

/// The user id a ticket names (its `uid` claim), or `None` when it can't be read.
#[must_use]
pub fn ticket_uid(ticket: &str) -> Option<String> {
    claims(ticket)?.get("uid")?.as_str().filter(|u| !u.is_empty()).map(str::to_owned)
}
