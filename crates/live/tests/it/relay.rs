//! Native clients converging through a real craft-collab relay. Ignored by default: they need a
//! relay and its secret, `CRAFT_TEST_RELAY` (for example `http://localhost:9444`, or the dev
//! stack's `http://<lan address>:8035/craft` through Apache) and `CRAFT_TEST_SECRET`, and run with
//! `--features native-hub -- --ignored relay`. `relay_restart_resumes` also needs
//! `CRAFT_TEST_RESTART_CMD`, a shell command that restarts that relay (for example
//! `docker restart it-relay`). `load_five_clients` takes `CRAFT_TEST_LOAD_SECS` (default 60).
//!
//! Neither the secret nor a ticket is ever printed: failures name clients and statuses only.

use std::collections::BTreeSet;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use craft_collab::native::NativeHub;
use craft_collab::{Hub, Replica};
use craft_live::{Access, LiveStatus};
use gridcraft_core::{CellRef, Value};
use gridcraft_engine::{Session, io};
use gridcraft_live::{BaseFile, GridLive, schema, volatile_for, workbook_from_state};
use gridcraft_model::{Cell, Formula, Workbook};
use hmac::digest::KeyInit;
use hmac::{Hmac, Mac};
use serde_json::{Value as Json, json};
use sha2::Sha256;

use crate::Rng;
use crate::fingerprint::{diff, finger};

const FILE: &str = "load.xlsx";

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} is not set"))
}

/// The wall clock. These tests run natively only (the native hub), where reading it is safe.
#[allow(clippy::disallowed_methods)] // off wasm only, as clippy.toml asks
fn wall() -> Duration {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default()
}

/// A monotonic instant, as [`wall`].
#[allow(clippy::disallowed_methods)] // off wasm only, as clippy.toml asks
fn mono() -> Instant {
    Instant::now()
}

fn now() -> u64 {
    wall().as_secs()
}

/// A join ticket as the relay's signer makes it: `base64url(claims).base64url(HMAC-SHA256)`.
fn ticket(secret: &str, room: &str, uid: &str, pid: &str) -> String {
    let claims = json!({"rm": room, "uid": uid, "dsp": uid, "perm": 3, "exp": now() + 900, "pid": pid});
    let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap());
    let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(payload.as_bytes());
    format!("{payload}.{}", URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes()))
}

/// A room no earlier run has used, so the relay starts the document from the file.
fn fresh_room() -> String {
    format!("it-grid-{}", wall().as_nanos())
}

/// The base file: a column of numbers and a sum over it.
fn base_bytes() -> Vec<u8> {
    let mut wb = Workbook::new();
    if let Some(s) = wb.sheet_mut(0) {
        for r in 0..8u32 {
            s.set_cell(CellRef::new(r, 0), Cell::value(Value::Number(f64::from(r + 1))));
        }
        s.set_cell(CellRef::new(0, 7), Cell::formula(Formula::new("=SUM(A1:A8)")));
    }
    io::save_bytes(&wb, FILE).unwrap()
}

struct Client {
    name: String,
    s: Session,
    live: GridLive<NativeHub>,
}

impl Client {
    async fn join(relay: &str, secret: &str, room: &str, uid: &str, file: &[u8]) -> Client {
        let token = ticket(secret, room, uid, &format!("{uid}-{}", now()));
        let schema = schema();
        let hub = NativeHub::connect_as(relay, &token, None, Some(&schema)).await.unwrap_or_else(|_| panic!("{uid}: the relay connection failed"));
        let mut s = Session::new();
        let base = BaseFile { bytes: file, name: FILE, expected_sha256: None };
        let live =
            GridLive::start(hub, &format!("doc:{room}"), &base, Access::Edit, uid, &mut s).unwrap_or_else(|e| panic!("{uid}: start failed: {e:?}"));
        Client { name: uid.to_owned(), s, live }
    }

    fn pump(&mut self) {
        self.live.pump(&mut self.s);
        // Notices are for a user; the tests read the status instead.
        let _ = self.live.take_notices();
    }

    fn is_live(&self) -> bool {
        matches!(self.live.status(), LiveStatus::Live { .. }) && self.live.editable()
    }

    fn run(&mut self, id: &str, params: Json) -> bool {
        self.live.run(&mut self.s, id, params).is_ok()
    }

    fn wb(&self) -> &Workbook {
        &self.s.active().unwrap().wb
    }

    fn finger(&self) -> String {
        finger(self.wb())
    }

    fn state_len(&self) -> usize {
        self.live.replica().map_or(0, |r| r.state().len())
    }
}

fn pump_all(cs: &mut [Client]) {
    for c in cs.iter_mut() {
        c.pump();
    }
}

fn state(cs: &[Client]) -> String {
    cs.iter()
        .map(|c| {
            let cells = c.wb().sheet(0).map_or(0, |s| s.cells.len());
            format!(
                "{}: {:?}, hub connected {}, editable {}, {cells} cells",
                c.name,
                c.live.status(),
                c.live.core().hub().inner().connected(),
                c.live.editable()
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Pump every client until `done` holds, or fail after `secs` with where each one stands.
async fn until(cs: &mut [Client], secs: u64, what: &str, done: impl Fn(&[Client]) -> bool) {
    let end = mono() + Duration::from_secs(secs);
    loop {
        pump_all(cs);
        if done(cs) {
            return;
        }
        assert!(mono() < end, "timed out waiting for {what}:\n{}", state(cs));
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn all_live(cs: &[Client]) -> bool {
    cs.iter().all(Client::is_live)
}

fn equal(cs: &[Client]) -> bool {
    let first = cs[0].finger();
    cs.iter().skip(1).all(|c| c.finger() == first)
}

/// Everyone is Live and equal, and stays so for `quiet`: nothing is still on its way. Then each
/// one also equals the base plus its own replica's state.
async fn converge(cs: &mut [Client], base: &Workbook, what: &str) {
    let quiet = Duration::from_millis(800);
    let mut since: Option<Instant> = None;
    let end = mono() + Duration::from_secs(60);
    loop {
        pump_all(cs);
        if all_live(cs) && equal(cs) {
            let t = *since.get_or_insert_with(mono);
            if t.elapsed() >= quiet {
                break;
            }
        } else {
            since = None;
        }
        if mono() >= end {
            let first = cs[0].finger();
            let first_diff = cs.iter().skip(1).map(|c| diff(&first, &c.finger())).find(|d| !d.is_empty()).unwrap_or_default();
            panic!("{what}: no convergence:\n{}\n{first_diff}", state(cs));
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    for c in cs.iter() {
        let st = c.live.replica().unwrap().state();
        let source = volatile_for(base, &st).unwrap();
        let rebuilt = finger(&workbook_from_state(base, &st, Some(source)).unwrap());
        assert!(c.finger() == rebuilt, "{what}: {} differs from its replica's state\n{}", c.name, diff(&c.finger(), &rebuilt));
    }
}

fn setup() -> (String, String, String, Vec<u8>) {
    (env("CRAFT_TEST_RELAY"), env("CRAFT_TEST_SECRET"), fresh_room(), base_bytes())
}

/// One random edit: mostly free writes, now and then a row or column insert or delete.
fn edit_somewhere(c: &mut Client, rng: &mut Rng, ordered: bool) {
    let row = 1 + rng.below(12);
    let col = (b'A' + u8::try_from(rng.below(6)).unwrap_or(0)) as char;
    let (id, params) = match rng.below(if ordered { 10 } else { 7 }) {
        0..=2 => ("cell.set", json!({"cell": format!("{col}{row}"), "input": format!("{}", rng.below(1000))})),
        3 => ("cell.set", json!({"cell": format!("{col}{row}"), "input": format!("=SUM(A1:A{row})")})),
        4 => ("home.bold", json!({"range": format!("{col}{row}:{col}{}", row + 1)})),
        5 => ("home.rowHeight", json!({"rows": format!("{row}:{row}"), "height": 20 + rng.below(30)})),
        6 => ("edit.clearContents", json!({"range": format!("{col}{row}")})),
        7 => ("home.insertRows", json!({"rows": format!("{row}:{row}")})),
        8 => ("home.deleteRows", json!({"rows": format!("{row}:{row}")})),
        _ => ("home.insertColumns", json!({"cols": format!("{col}:{col}")})),
    };
    c.run(id, params);
}

async fn three(relay: &str, secret: &str, room: &str, file: &[u8]) -> Vec<Client> {
    vec![
        Client::join(relay, secret, room, "alice", file).await,
        Client::join(relay, secret, room, "bob", file).await,
        Client::join(relay, secret, room, "carol", file).await,
    ]
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs relay"]
async fn three_native_clients_converge_through_relay() {
    let (relay, secret, room, file) = setup();
    let base = io::open_bytes(FILE, &file).unwrap().0;
    let mut cs = three(&relay, &secret, &room, &file).await;
    until(&mut cs, 20, "every client Live", all_live).await;

    // Concurrent random edits, ordered ones among them, a few pumps between them so some
    // overlap and some do not.
    let mut rng = Rng(7);
    for step in 0..150 {
        let k = rng.below(cs.len());
        edit_somewhere(&mut cs[k], &mut rng, true);
        if step % 3 == 0 {
            pump_all(&mut cs);
        }
        if step % 10 == 0 {
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
    }
    converge(&mut cs, &base, "after the random edits").await;
    assert!(cs[0].finger() != finger(&base), "the edits landed");

    // A late joiner reaches the same workbook from the relay's catch-up.
    let late = Client::join(&relay, &secret, &room, "dave", &file).await;
    cs.push(late);
    until(&mut cs, 20, "the late joiner Live", all_live).await;
    converge(&mut cs, &base, "with the late joiner").await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs relay"]
async fn relay_restart_resumes() {
    let (relay, secret, room, file) = setup();
    let restart = env("CRAFT_TEST_RESTART_CMD");
    let base = io::open_bytes(FILE, &file).unwrap().0;
    let mut cs = three(&relay, &secret, &room, &file).await;
    until(&mut cs, 20, "every client Live", all_live).await;
    let mut rng = Rng(11);
    for _ in 0..30 {
        let k = rng.below(cs.len());
        edit_somewhere(&mut cs[k], &mut rng, true);
        pump_all(&mut cs);
    }
    // A mark from each client, in a cell of its own, shared before the restart: it must survive.
    for (k, c) in cs.iter_mut().enumerate() {
        assert!(c.run("cell.set", json!({"cell": format!("J{}", k + 1), "input": format!("pre{k}")})), "client {k} writes before the restart");
    }
    converge(&mut cs, &base, "before the restart").await;

    // Restart the relay while the clients keep editing (free writes only); edits made while not
    // live are refused.
    let child = std::thread::spawn(move || std::process::Command::new("sh").arg("-c").arg(restart).output());
    let (mut tried_live, mut tried_down) = (0usize, 0usize);
    while !child.is_finished() {
        for c in cs.iter_mut() {
            if c.is_live() {
                tried_live += 1
            } else {
                tried_down += 1
            }
            edit_somewhere(c, &mut rng, false);
        }
        pump_all(&mut cs);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let out = child.join().unwrap().unwrap();
    assert!(out.status.success(), "the restart command failed");
    eprintln!("during the restart: {tried_live} edit attempts while live, {tried_down} while not live");
    eprintln!("after the restart:\n{}", state(&cs));

    for c in &cs {
        assert!(!matches!(c.live.status(), LiveStatus::Broken(_)), "{} is Broken after the restart: {:?}", c.name, c.live.status());
    }
    until(&mut cs, 60, "every client Live again", all_live).await;
    converge(&mut cs, &base, "after the restart").await;
    let marks: BTreeSet<String> = cs[0].wb().sheet(0).map(|s| s.cells.iter().map(|(_, c)| format!("{:?}", c.value)).collect()).unwrap_or_default();
    for k in 0..3 {
        assert!(marks.iter().any(|v| v.contains(&format!("pre{k}"))), "client {k}'s mark survived the restart");
    }

    // Editing carries on, ordered edits included.
    assert!(cs[0].run("home.insertRows", json!({"rows": "1:1"})), "an ordered edit after the restart");
    for (k, c) in cs.iter_mut().enumerate() {
        assert!(c.run("cell.set", json!({"cell": format!("K{}", k + 1), "input": format!("post{k}")})), "client {k} can edit again");
    }
    converge(&mut cs, &base, "after editing again").await;
}

/// The load check: 5 clients, each making 4 cell writes a second, plus one row insert every 2 s
/// (by each client in turn), for 60 s (`CRAFT_TEST_LOAD_SECS`). Each client times every other
/// client's writes, from just before the writer's `run` until the value is in its own workbook
/// (remote apply). The target is a 95th percentile under 200 ms; Ruling R20: it is recorded, not
/// asserted. Also printed: the overlay's state size (the replica's encoded state) at the end.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs relay"]
async fn load_five_clients() {
    const N: usize = 5;
    const WRITE_EVERY: Duration = Duration::from_millis(250);
    const INSERT_EVERY: Duration = Duration::from_secs(2);
    let secs: u64 = std::env::var("CRAFT_TEST_LOAD_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(60);
    let (relay, secret, room, file) = setup();
    let base = io::open_bytes(FILE, &file).unwrap().0;
    let mut cs = Vec::new();
    for k in 0..N {
        cs.push(Client::join(&relay, &secret, &room, &format!("writer{k}"), &file).await);
    }
    until(&mut cs, 30, "every client Live", all_live).await;

    let writes = usize::try_from(secs * 4).unwrap_or(usize::MAX);
    let inserts = usize::try_from(secs / 2).unwrap_or(usize::MAX);
    // Client k writes only column B + k, each time one row below its lowest value, so its
    // values never overwrite each other even as inserts move rows: each token is unique.
    let token = |k: usize, i: usize| format!("w{k}_{i}");
    let tokens_in = |c: &Client| -> BTreeSet<String> {
        c.wb()
            .sheet(0)
            .map(|s| s.cells.iter().filter_map(|(_, cell)| if let Value::Text(t) = &cell.value { Some(t.to_string()) } else { None }).collect())
            .unwrap_or_default()
    };
    let next_row = |c: &Client, k: usize| -> u32 {
        let col = u32::try_from(k + 1).unwrap_or(1);
        c.wb().sheet(0).map_or(0, |s| s.cells.iter().filter(|(at, _)| at.col == col).map(|(at, _)| at.row + 1).max().unwrap_or(0))
    };
    // sent[k][i]: when client k started its write i.
    let mut sent: Vec<Vec<Instant>> = (0..N).map(|_| Vec::with_capacity(writes)).collect();
    // seen[j]: the tokens client j has been timed on.
    let mut seen: Vec<BTreeSet<String>> = (0..N).map(|_| BTreeSet::new()).collect();
    let mut ms = Vec::with_capacity(N * (N - 1) * writes);
    let (mut refused, mut inserted, mut insert_refused) = (0usize, 0usize, 0usize);
    let mut slowest_lap = Duration::ZERO;
    let start = mono() + Duration::from_millis(200);
    let give_up = start + Duration::from_secs(secs + 60);
    loop {
        let lap = mono();
        for k in 0..N {
            // Writers spread over the period, so not all five write in the same instant.
            let due = start + WRITE_EVERY * u32::try_from(sent[k].len()).unwrap_or(u32::MAX) + Duration::from_millis(50 * k as u64);
            if sent[k].len() < writes && mono() >= due {
                let i = sent[k].len();
                let row = next_row(&cs[k], k);
                let at = CellRef::new(row, u32::try_from(k + 1).unwrap_or(1)).a1();
                let t = mono();
                if cs[k].run("cell.set", json!({"cell": at, "input": token(k, i)})) {
                    sent[k].push(t);
                } else {
                    refused += 1;
                }
            }
        }
        if inserted + insert_refused < inserts && mono() >= start + INSERT_EVERY * u32::try_from(inserted + insert_refused + 1).unwrap_or(u32::MAX) {
            let k = (inserted + insert_refused) % N;
            let row = 1 + (inserted % 10);
            if cs[k].run("home.insertRows", json!({"rows": format!("{row}:{row}")})) { inserted += 1 } else { insert_refused += 1 }
        }
        pump_all(&mut cs);
        let now = mono();
        for j in 0..N {
            for tok in tokens_in(&cs[j]) {
                if seen[j].contains(&tok) {
                    continue;
                }
                let Some((k, i)) = tok
                    .strip_prefix('w')
                    .and_then(|r| r.split_once('_'))
                    .and_then(|(a, b)| Some((a.parse::<usize>().ok()?, b.parse::<usize>().ok()?)))
                else {
                    continue;
                };
                if k != j
                    && k < N
                    && let Some(t) = sent[k].get(i)
                {
                    ms.push((now - *t).as_secs_f64() * 1000.0);
                }
                seen[j].insert(tok);
            }
        }
        slowest_lap = slowest_lap.max(lap.elapsed());
        let done_writing = sent.iter().all(|s| s.len() == writes) && inserted + insert_refused == inserts;
        let total: usize = sent.iter().map(Vec::len).sum();
        if done_writing && seen.iter().all(|s| s.len() == total) {
            break;
        }
        assert!(mono() < give_up, "not every write arrived:\n{}", state(&cs));
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    converge(&mut cs, &base, "after the load").await;

    ms.sort_by(f64::total_cmp);
    assert!(!ms.is_empty(), "remote applies were timed");
    let at = |p: f64| ms[((ms.len() as f64 * p).ceil() as usize).saturating_sub(1).min(ms.len() - 1)];
    let sizes: Vec<usize> = cs.iter().map(Client::state_len).collect();
    eprintln!(
        "load: {N} clients x {writes} cell writes + {inserted} row inserts ({insert_refused} refused) over {secs} s; {} remote applies: min {:.1} ms, p50 {:.1} ms, p95 {:.1} ms (target 200 ms, recorded not asserted), p99 {:.1} ms, max {:.1} ms; writes refused {refused}; slowest pump lap {:.1} ms",
        ms.len(),
        ms[0],
        at(0.5),
        at(0.95),
        at(0.99),
        ms[ms.len() - 1],
        slowest_lap.as_secs_f64() * 1000.0
    );
    eprintln!("load: overlay state size {} bytes (per client {sizes:?})", sizes.iter().max().copied().unwrap_or(0));
}
