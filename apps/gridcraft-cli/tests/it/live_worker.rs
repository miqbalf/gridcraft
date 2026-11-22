//! `live-worker`'s document service against a peer in an in-memory room: what the worker saves
//! is what the editors see.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

// The service is part of the binary; the tests compile the same file.
#[path = "../../src/live_worker.rs"]
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

struct Rig {
    relay: MemRelay,
    peer: GridLive<MemHub>,
    session: Session,
    doc: GridDoc<MemHub>,
}

impl Rig {
    fn new(bytes: &[u8]) -> Rig {
        let relay = MemRelay::new();
        let hub = relay.connect("alice", "Alice", true);
        let mut session = Session::new();
        let base = BaseFile { bytes, name: FILE, expected_sha256: None };
        let peer = GridLive::start(hub, ROOM, &base, Access::Edit, "alice", &mut session).unwrap();
        let worker_hub = relay.connect(SERVICE_UID, "Document service", true);
        let doc = GridService.open(worker_hub, ROOM, FILE, bytes).unwrap();
        let mut rig = Rig { relay, peer, session, doc };
        rig.settle();
        rig
    }

    fn settle(&mut self) {
        for _ in 0..60 {
            self.peer.pump(&mut self.session);
            self.doc.pump();
            if self.relay.quiet() {
                self.peer.pump(&mut self.session);
                self.doc.pump();
                if self.relay.quiet() {
                    return;
                }
            }
        }
    }

    fn run(&mut self, id: &str, params: Json) {
        self.peer.run(&mut self.session, id, params).unwrap();
        self.settle();
    }

    fn wb(&self) -> Arc<Workbook> {
        self.session.active().unwrap().wb.clone()
    }

    /// The worker's checkpoint, written as a file.
    fn saved(&mut self) -> Vec<u8> {
        let state = self.doc.live_mut().checkpoint().unwrap();
        self.settle();
        self.doc.save(&state, FILE).unwrap()
    }
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
    let before = rig.doc.snapshot();
    rig.settle();
    let idle = rig.doc.snapshot();
    assert!(rig.doc.unchanged(&before, &idle), "pumping an idle room changes no sheet");
    assert!(rig.doc.same(&before, &idle));
    rig.run("cell.set", json!({"cell": "E1", "input": "x"}));
    let after = rig.doc.snapshot();
    assert!(!rig.doc.unchanged(&before, &after), "an edit replaces a sheet");
    assert!(!rig.doc.same(&before, &after), "and the content differs");
}
