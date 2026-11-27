//! The launcher's session file (`--live-session`): what is read from it, what is refused, and
//! the rewrites that carry a fresh ticket.

use std::path::{Path, PathBuf};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use craft_live::Access;
use gridcraft_live::desktop::{ACCESS_CHANGED_TEXT, RESTARTED_TEXT, Rewrite, SessionError, SessionFile, SessionWatch, WATCH_SECS};
use serde_json::{Value as Json, json};
use sha2::{Digest, Sha256};

fn ticket(uid: &str, perm: u64, n: u32) -> String {
    let claims = json!({"rm": "oc:1", "uid": uid, "dsp": uid, "perm": perm, "exp": 1_900_000_000 + n, "pid": format!("{uid}~{n}")});
    format!("{}.c2ln", URL_SAFE_NO_PAD.encode(claims.to_string()))
}

fn hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

fn session(extra: Json) -> Json {
    let mut j = json!({
        "v": 1, "server": "https://cloud.example", "fileId": 7, "path": "/Docs/Budget.xlsx",
        "ticket": ticket("ann", 3, 0), "relayUrl": "wss://cloud.example/craft", "room": "oc:7:abc",
        "expiresAt": "2026-11-27T12:00:00Z", "readOnly": false, "displayName": "Ann",
        "base": {"sha256": hex(b"base")}
    });
    if let (Some(map), Some(more)) = (j.as_object_mut(), extra.as_object()) {
        for (k, v) in more {
            map.insert(k.clone(), v.clone());
        }
    }
    j
}

fn parse(j: &Json) -> Result<SessionFile, SessionError> {
    SessionFile::parse(j.to_string().as_bytes())
}

/// A directory of this test's own under the system temp folder.
fn dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("gridcraft-live-desktop-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Writes as the launcher does: a temporary file, then a rename over the old one.
fn write_atomic(path: &Path, j: &Json) {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, j.to_string()).unwrap();
    std::fs::rename(&tmp, path).unwrap();
}

#[test]
fn unknown_fields_are_ignored_and_an_unknown_version_is_refused() {
    let file = parse(&session(json!({"futureField": {"x": 1}}))).expect("unknown fields are ignored");
    assert_eq!(file.room, "oc:7:abc");
    assert_eq!(parse(&session(json!({"v": 2}))), Err(SessionError::Newer), "a newer launcher falls back to a plain open");
    assert_eq!(parse(&session(json!({"ticket": ""}))), Err(SessionError::Malformed));
    assert_eq!(SessionFile::parse(b"not json"), Err(SessionError::Malformed));
}

#[test]
fn the_ticket_never_shows_in_debug_or_errors() {
    let file = parse(&session(json!({}))).unwrap();
    let shown = format!("{file:?}");
    assert!(!shown.contains(&file.ticket), "Debug leaves the ticket out");
    for e in [SessionError::Malformed, SessionError::RelayElsewhere, SessionError::BaseChanged] {
        assert!(!e.to_string().contains(&file.ticket));
    }
}

#[test]
fn read_only_comes_from_the_launcher_or_the_ticket() {
    assert_eq!(parse(&session(json!({}))).unwrap().access(), Access::Edit);
    assert_eq!(parse(&session(json!({"readOnly": true}))).unwrap().access(), Access::Read);
    assert_eq!(parse(&session(json!({"ticket": ticket("ann", 1, 0)}))).unwrap().access(), Access::Read, "a read ticket stays read-only");
}

#[test]
fn a_room_without_a_save_worker_is_told() {
    assert!(parse(&session(json!({}))).unwrap().saving(), "missing means saving");
    assert!(!parse(&session(json!({"saver": false}))).unwrap().saving());
    assert!(parse(&session(json!({"saver": false, "readOnly": true}))).unwrap().saving(), "a viewer saves nothing anyway");
}

#[test]
fn the_relay_must_be_on_the_servers_own_address() {
    let relay = |server: &str, url: &str| parse(&session(json!({"server": server, "relayUrl": url}))).unwrap().relay();
    assert_eq!(relay("https://cloud.example", "wss://cloud.example/craft").as_deref(), Ok("https://cloud.example/craft"));
    assert_eq!(relay("https://cloud.example", "/craft").as_deref(), Ok("https://cloud.example/craft"), "a path is on the server");
    assert_eq!(relay("http://192.168.1.12:8035", "ws://192.168.1.12:8035/craft").as_deref(), Ok("http://192.168.1.12:8035/craft"));
    assert!(relay("https://cloud.example:443", "wss://Cloud.Example/craft").is_ok(), "default ports and case are the same address");
    for (server, url) in [
        ("https://cloud.example", "wss://relay.evil.example/craft"),
        ("https://cloud.example", "wss://cloud.example:8443/craft"),
        ("https://cloud.example", "ws://cloud.example/craft"),
        ("https://cloud.example", "//evil.example/craft"),
        ("https://cloud.example", "wss://user@cloud.example/craft"),
        ("https://cloud.example", "javascript:alert(1)"),
        ("file:///etc", "/craft"),
    ] {
        assert_eq!(relay(server, url), Err(SessionError::RelayElsewhere), "{server} {url}");
    }
}

#[test]
fn the_room_opens_only_on_its_pinned_base() {
    let d = dir("base");
    let working = d.join("Budget.xlsx");
    std::fs::write(&working, b"base").unwrap();
    let file = parse(&session(json!({}))).unwrap();
    let (bytes, _) = file.base_bytes(&working).expect("the working copy is the base");
    assert_eq!(bytes, b"base");
    std::fs::write(&working, b"edited since").unwrap();
    assert_eq!(file.base_bytes(&working).map(|_| ()), Err(SessionError::BaseChanged), "another file is not joined");
    let base = d.join("base.bin");
    std::fs::write(&base, b"base").unwrap();
    let with_path = parse(&session(json!({"base": {"sha256": hex(b"base"), "path": base}}))).unwrap();
    assert!(with_path.base_bytes(&working).is_ok(), "a separately downloaded base is used");
    let prod = parse(&session(json!({"app": "gridcraft", "base": {"url": "https://cloud.example/apps/craft_office/live/base/r?sha256=x", "sha256": hex(b"base")}}))).unwrap();
    assert!(prod.base_bytes(&working).is_ok(), "the ticket's base, url and all, is read for its hash");
    let none = parse(&session(json!({"base": null}))).unwrap();
    assert_eq!(none.base_bytes(&working).map(|_| ()), Err(SessionError::NoBase), "no hash, no join");
    assert_eq!(file.file_name(&working), "Budget.xlsx");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_rewrite_with_a_fresh_ticket_is_seen_and_other_rewrites_end_the_session() {
    let d = dir("watch");
    let path = d.join("s.json");
    let first = session(json!({}));
    write_atomic(&path, &first);
    let file = SessionFile::load(&path).unwrap();
    let mut watch = SessionWatch::new(&path, &file);
    let mut now = 0.0;
    let mut look = |watch: &mut SessionWatch| {
        now += WATCH_SECS;
        watch.poll(now)
    };
    assert_eq!(look(&mut watch), None, "unchanged");
    // Some file systems keep whole seconds: wait so the rewrite has a later time.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let fresh = ticket("ann", 3, 1);
    write_atomic(&path, &session(json!({"ticket": fresh})));
    assert_eq!(look(&mut watch), Some(Rewrite::Ticket(fresh.clone())));
    assert_eq!(look(&mut watch), None, "seen once");
    std::thread::sleep(std::time::Duration::from_millis(1100));
    std::fs::write(&path, b"{\"v\":1,").unwrap();
    assert_eq!(look(&mut watch), None, "a half-written file is read again later");
    std::thread::sleep(std::time::Duration::from_millis(1100));
    write_atomic(&path, &session(json!({"ticket": fresh, "readOnly": true})));
    assert_eq!(look(&mut watch), Some(Rewrite::End(ACCESS_CHANGED_TEXT)));
    std::thread::sleep(std::time::Duration::from_millis(1100));
    write_atomic(&path, &session(json!({"room": "oc:7:def"})));
    assert_eq!(look(&mut watch), Some(Rewrite::End(RESTARTED_TEXT)));
    std::fs::remove_file(&path).unwrap();
    assert_eq!(look(&mut watch), None, "a deleted file means nothing");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn the_file_is_looked_at_no_more_than_every_two_seconds() {
    let d = dir("cadence");
    let path = d.join("s.json");
    write_atomic(&path, &session(json!({})));
    let file = SessionFile::load(&path).unwrap();
    let mut watch = SessionWatch::new(&path, &file);
    assert_eq!(watch.poll(10.0), None);
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let fresh = ticket("ann", 3, 2);
    write_atomic(&path, &session(json!({"ticket": fresh})));
    assert_eq!(watch.poll(10.0 + WATCH_SECS / 2.0), None, "too soon");
    assert_eq!(watch.poll(10.0 + WATCH_SECS), Some(Rewrite::Ticket(fresh)));
    let _ = std::fs::remove_dir_all(&d);
}
