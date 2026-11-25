//! `live-worker`'s document service against a peer in an in-memory room: what the worker saves
//! is what the editors see.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

// The service is part of the binary; the tests compile the same file.
#[path = "../../src/live_worker.rs"]
#[allow(dead_code)]
mod service;

use std::sync::Arc;

use craft_live::testing::{MemHub, MemRelay, ROOM};
use craft_live::{Access, SERVICE_UID};
use craft_live_worker::{DocumentService, LiveDocument};
use gridcraft_core::{CellRef, Value};
use gridcraft_engine::{Session, io};
use gridcraft_live::{BaseFile, GridLive};
use gridcraft_model::{Cell, Formula, Workbook};
use serde_json::{Value as Json, json};
use service::{GridDoc, GridService};

const FILE: &str = "book.xlsx";

fn base_bytes(extra: impl FnOnce(&mut Workbook)) -> Vec<u8> {
    let mut wb = Workbook::new();
    if let Some(s) = wb.sheet_mut(0) {
        for r in 0..8u32 {
            s.set_cell(CellRef::new(r, 0), Cell::value(Value::Number(f64::from(r + 1))));
            s.set_cell(CellRef::new(r, 2), Cell::value(Value::Text(format!("xx {r}").into())));
        }
        s.set_cell(CellRef::new(0, 1), Cell::formula(Formula::new("=SUM(A1:A8)")));
    }
    extra(&mut wb);
    io::save_bytes(&wb, FILE).unwrap()
}

struct Peer {
    live: GridLive<MemHub>,
    session: Session,
}

struct Rig {
    relay: MemRelay,
    peers: Vec<Peer>,
    doc: Option<GridDoc<MemHub>>,
    bytes: Vec<u8>,
}

impl Rig {
    /// `n` editors and the worker, all joined at the start.
    fn new(bytes: &[u8]) -> Rig {
        Rig::with(bytes, 1, false)
    }

    /// `n` editors; the worker joins at once, or only when `late` is called.
    fn with(bytes: &[u8], n: usize, worker_late: bool) -> Rig {
        let mut rig = Rig { relay: MemRelay::new(), peers: Vec::new(), doc: None, bytes: bytes.to_vec() };
        for name in ["alice", "bob", "carol"].into_iter().take(n) {
            let hub = rig.relay.connect(name, name, true);
            let mut session = Session::new();
            let base = BaseFile { bytes, name: FILE, expected_sha256: None };
            let live = GridLive::start(hub, ROOM, &base, Access::Edit, name, &mut session).unwrap();
            rig.peers.push(Peer { live, session });
        }
        if !worker_late {
            rig.join_worker();
        }
        rig.settle();
        rig
    }

    fn join_worker(&mut self) {
        let hub = self.relay.connect(SERVICE_UID, "Document service", true);
        self.doc = Some(GridService.open(hub, ROOM, FILE, &self.bytes).unwrap());
    }

    fn doc(&mut self) -> &mut GridDoc<MemHub> {
        self.doc.as_mut().unwrap()
    }

    fn pump_all(&mut self) {
        for p in &mut self.peers {
            p.live.pump(&mut p.session);
        }
        if let Some(d) = self.doc.as_mut() {
            d.pump();
        }
    }

    fn settle(&mut self) {
        for _ in 0..80 {
            self.pump_all();
            if self.relay.quiet() {
                self.pump_all();
                if self.relay.quiet() {
                    return;
                }
            }
        }
    }

    /// Peer `i` runs a command, and everyone settles.
    fn run_on(&mut self, i: usize, id: &str, params: Json) {
        self.run_now(i, id, params);
        self.settle();
    }

    /// Peer `i` runs a command; nothing is delivered yet.
    fn run_now(&mut self, i: usize, id: &str, params: Json) {
        let p = &mut self.peers[i];
        p.live.run(&mut p.session, id, params).unwrap();
    }

    fn run(&mut self, id: &str, params: Json) {
        self.run_on(0, id, params);
    }

    fn wb_of(&self, i: usize) -> Arc<Workbook> {
        self.peers[i].session.active().unwrap().wb.clone()
    }

    fn wb(&self) -> Arc<Workbook> {
        self.wb_of(0)
    }

    /// The worker's checkpoint, written as a file.
    fn saved(&mut self) -> Vec<u8> {
        let state = self.doc().live_mut().checkpoint().unwrap();
        self.settle();
        self.doc().save(&state, FILE).unwrap()
    }

    /// The saved file shows what every editor shows.
    fn assert_saved_equals_peers(&mut self, what: &str) {
        let saved = self.saved();
        let (reopened, _) = io::open_bytes(FILE, &saved).unwrap();
        let got = finger(&reopened);
        for i in 0..self.peers.len() {
            let want = finger(&self.wb_of(i));
            assert!(want == got, "{what}: the saved file differs from peer {i}\n{}", first_diff(&want, &got));
        }
    }
}

fn first_diff(a: &str, b: &str) -> String {
    a.lines()
        .zip(b.lines())
        .enumerate()
        .find(|(_, (x, y))| x != y)
        .map_or_else(|| format!("lengths {} and {}", a.lines().count(), b.lines().count()), |(i, (x, y))| format!("line {i}: {x} | {y}"))
}

/// Values, formulas, style content and line info of every sheet.
fn finger(wb: &Workbook) -> String {
    let mut out = String::new();
    for (i, s) in wb.sheets.iter().enumerate() {
        out.push_str(&format!("sheet {i}\n"));
        let mut cells: Vec<(CellRef, &Cell)> = s.cells.iter().collect();
        cells.sort_by_key(|(c, _)| *c);
        for (c, cell) in cells {
            let f = cell.formula.as_ref().map(|f| f.text.clone());
            let style = serde_json::to_string(wb.styles.get(cell.style)).unwrap();
            out.push_str(&format!("{} {:?} {:?} {style}\n", c.a1(), cell.value, f));
        }
        let lines = |m: &std::collections::BTreeMap<u32, gridcraft_model::LineInfo>| {
            m.iter()
                .filter(|(_, l)| **l != Default::default())
                .map(|(k, l)| {
                    (*k, gridcraft_model::LineInfo { style: None, ..*l }, l.style.map(|id| serde_json::to_string(wb.styles.get(id)).unwrap()))
                })
                .collect::<Vec<_>>()
        };
        out.push_str(&format!("rows {:?}\ncols {:?}\n", lines(&s.rows), lines(&s.cols)));
    }
    out
}

fn a1(s: &str) -> CellRef {
    CellRef::parse(s).unwrap()
}

#[test]
fn accepts_only_xlsx() {
    assert!(GridService.accepts("book.xlsx"));
    for name in ["book.xls", "book.xlsb", "book.ods", "book.csv", "book.xlsm"] {
        assert!(!GridService.accepts(name), "{name} is not saved live");
    }
    assert!(GridService.pinned_base());
}

#[test]
fn saved_file_reopens_equal_to_the_live_workbook() {
    let mut rig = Rig::new(&base_bytes(|_| {}));
    rig.run("cell.set", json!({"cell": "E1", "input": "hello"}));
    rig.run("cell.set", json!({"cell": "E2", "input": "=SUM(A1:A3)"}));
    rig.run("home.bold", json!({"range": "C2:C3"}));
    rig.run("home.fillColor", json!({"range": "A1", "color": "#FF8800"}));
    rig.run("home.insertRows", json!({"rows": "2:2"}));
    rig.run("cell.set", json!({"cell": "A2", "input": "9"}));
    let saved = rig.saved();
    let (reopened, _) = io::open_bytes(FILE, &saved).unwrap();
    let want = finger(&rig.wb());
    let got = finger(&reopened);
    assert_eq!(want, got, "the saved file shows what the editor shows");
    assert!(want.contains("hello"), "the edit is in the comparison");
}

#[test]
fn save_keeps_unmodelled_parts_from_the_base() {
    use gridcraft_model::preserved::{Preserved, PreservedPart, PreservedRel};
    let data: Arc<[u8]> = Arc::from(&b"<keep xmlns=\"urn:keep\">custom</keep>"[..]);
    let bytes = base_bytes(|wb| {
        wb.preserved = Some(Arc::new(Preserved {
            rels: vec![PreservedRel {
                id: "rId90".into(),
                rel_type: "http://schemas.openxmlformats.org/officeDocument/2006/relationships/customXml".into(),
                target: "customXml/item1.xml".into(),
                external: false,
            }],
            parts: vec![PreservedPart {
                name: "customXml/item1.xml".into(),
                content_type: "application/xml".into(),
                data: data.clone(),
                rels: vec![],
            }],
            ..Preserved::default()
        }));
    });
    let held = |b: &[u8]| -> Option<Vec<u8>> {
        let (wb, _) = io::open_bytes(FILE, b).unwrap();
        wb.preserved.as_ref()?.parts.iter().find(|p| p.name.ends_with("item1.xml")).map(|p| p.data.to_vec())
    };
    assert_eq!(held(&bytes).as_deref(), Some(&data[..]), "the base itself carries the part");
    let mut rig = Rig::new(&bytes);
    rig.run("cell.set", json!({"cell": "E1", "input": "7"}));
    let saved = rig.saved();
    assert_eq!(held(&saved).as_deref(), Some(&data[..]), "the part survives the save");
}

#[test]
fn rand_in_the_saved_file_equals_the_peers() {
    let mut rig = Rig::new(&base_bytes(|_| {}));
    rig.run("cell.set", json!({"cell": "G1", "input": "=RAND()"}));
    rig.run("cell.set", json!({"cell": "G2", "input": "=RANDBETWEEN(1,1000000)"}));
    let saved = rig.saved();
    let (reopened, _) = io::open_bytes(FILE, &saved).unwrap();
    for cell in ["G1", "G2"] {
        let (Value::Number(x), Value::Number(y)) =
            (rig.wb().sheet(0).unwrap().cell(a1(cell)).unwrap().value.clone(), reopened.sheet(0).unwrap().cell(a1(cell)).unwrap().value.clone())
        else {
            panic!("{cell} is not a number")
        };
        assert_eq!(x.to_bits(), y.to_bits(), "{cell}: the saved number is the one the peer sees");
    }
}

#[test]
fn an_unchanged_room_saves_nothing() {
    let mut rig = Rig::new(&base_bytes(|_| {}));
    let before = rig.doc().snapshot();
    rig.settle();
    let idle = rig.doc().snapshot();
    assert!(rig.doc().unchanged(&before, &idle), "pumping an idle room changes no sheet");
    assert!(rig.doc().same(&before, &idle));
    rig.run("cell.set", json!({"cell": "E1", "input": "x"}));
    let after = rig.doc().snapshot();
    assert!(!rig.doc().unchanged(&before, &after), "an edit replaces a sheet");
    assert!(!rig.doc().same(&before, &after), "and the content differs");
}

#[test]
fn saved_file_equals_the_peers_after_deletes_and_column_inserts() {
    let mut rig = Rig::new(&base_bytes(|_| {}));
    rig.run("cell.set", json!({"cell": "E3", "input": "=A3*2"}));
    rig.run("home.deleteRows", json!({"rows": "3:3"}));
    rig.run("home.insertColumns", json!({"cols": "B:B"}));
    rig.run("cell.set", json!({"cell": "B1", "input": "new"}));
    rig.run("home.deleteColumns", json!({"cols": "E:E"}));
    rig.run("home.insertRows", json!({"rows": "1:2"}));
    rig.assert_saved_equals_peers("deletes and column inserts");
}

#[test]
fn saved_file_equals_the_peers_in_line_styles_sizes_hidden_and_formats() {
    let mut rig = Rig::new(&base_bytes(|_| {}));
    rig.run("home.rowHeight", json!({"rows": "2:2", "height": 44}));
    rig.run("home.columnWidth", json!({"cols": "C:C", "width": 130}));
    rig.run("home.hideRows", json!({"rows": "4:4"}));
    rig.run("home.hideColumns", json!({"cols": "D:D"}));
    rig.run("home.bold", json!({"range": "6:6"}));
    rig.run("home.fillColor", json!({"range": "B:B", "color": "#33AA55"}));
    rig.run("home.numberFormat", json!({"range": "A1:A3", "code": "0.0%"}));
    rig.run("home.numberFormat", json!({"range": "C1", "code": "YYYY-MM-DD"}));
    rig.run("home.insertRows", json!({"rows": "3:3"}));
    let want = finger(&rig.wb());
    assert!(want.contains("rows [("), "the comparison holds line info: {want}");
    rig.assert_saved_equals_peers("line styles, sizes, hidden, formats");
}

#[test]
fn rand_after_insert_and_delete_equals_the_peers() {
    let mut rig = Rig::with(&base_bytes(|_| {}), 2, false);
    rig.run_on(0, "cell.set", json!({"cell": "G4", "input": "=RAND()"}));
    rig.run_on(1, "cell.set", json!({"cell": "H4", "input": "=RANDBETWEEN(1,1000000)"}));
    rig.run_on(0, "home.insertRows", json!({"rows": "2:3"}));
    rig.run_on(1, "home.deleteRows", json!({"rows": "1:1"}));
    rig.run_on(0, "home.insertColumns", json!({"cols": "A:A"}));
    rig.run_on(1, "home.deleteColumns", json!({"cols": "C:C"}));
    rig.assert_saved_equals_peers("RAND after inserts and deletes");
    let (reopened, _) = io::open_bytes(FILE, &rig.saved()).unwrap();
    let numbers = reopened.sheet(0).unwrap().cells.iter().filter(|(_, c)| c.formula.as_ref().is_some_and(|f| f.text.contains("RAND"))).count();
    assert_eq!(numbers, 2, "both random cells are in the saved file");
}

#[test]
fn a_worker_joining_a_room_with_history_saves_what_the_peers_see() {
    let mut rig = Rig::with(&base_bytes(|_| {}), 2, true);
    rig.run_on(0, "cell.set", json!({"cell": "E1", "input": "early"}));
    rig.run_on(1, "home.insertRows", json!({"rows": "2:2"}));
    rig.run_on(0, "home.bold", json!({"range": "C1:C2"}));
    rig.run_on(1, "cell.set", json!({"cell": "G1", "input": "=RAND()"}));
    rig.join_worker();
    rig.settle();
    rig.run_on(0, "cell.set", json!({"cell": "E2", "input": "late"}));
    rig.assert_saved_equals_peers("a late worker");
}

#[test]
fn two_peers_editing_at_once_while_the_worker_saves() {
    let mut rig = Rig::with(&base_bytes(|_| {}), 2, false);
    for round in 0..6 {
        rig.run_now(0, "cell.set", json!({"cell": format!("E{}", round + 1), "input": format!("a{round}")}));
        rig.run_now(1, "cell.set", json!({"cell": format!("F{}", round + 1), "input": format!("{round}")}));
        rig.run_now(1, "home.fillColor", json!({"range": format!("C{}", round + 1), "color": "#FFCC00"}));
        // A save in the middle of the traffic: the file must be a state the room reaches.
        let state = rig.doc().live_mut().checkpoint().unwrap();
        rig.pump_all();
        let mid = rig.doc().save(&state, FILE).unwrap();
        assert!(io::open_bytes(FILE, &mid).is_ok(), "a save taken mid-traffic opens");
        if round == 2 {
            rig.run_now(0, "home.insertRows", json!({"rows": "2:2"}));
        }
        rig.settle();
    }
    rig.assert_saved_equals_peers("two peers at once");
}

#[test]
fn save_refuses_when_the_session_ended() {
    let mut rig = Rig::new(&base_bytes(|_| {}));
    rig.run("cell.set", json!({"cell": "E1", "input": "x"}));
    let state = rig.doc().live_mut().checkpoint().unwrap();
    rig.settle();
    assert!(rig.doc().save(&state, FILE).is_ok(), "a healthy session saves");
    rig.doc().live_mut().end("test");
    let refused = rig.doc().save(&state, FILE).unwrap_err();
    assert!(refused.starts_with("not saved"), "a session that ended never writes the file: {refused}");
}
