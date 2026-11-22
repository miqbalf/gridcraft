//! A harness of two to eight peers in one in-memory room: each has its own engine session and a
//! [`GridLive`] on a `craft_live::testing::MemRelay`. Delivery order is the order the test pumps
//! the peers in, which the tests draw from a seeded generator.

use std::cell::RefCell;
use std::rc::Rc;

use craft_collab::{ClientMsg, Hub, ServerMsg};
use craft_live::Access;
use craft_live::testing::{MemHub, MemRelay, ROOM};
use gridcraft_core::{CellRef, RangeRef, Value};
use gridcraft_engine::{Session, io};
use gridcraft_live::styles::StyleHash;
use gridcraft_live::{BaseFile, GridLive, Ran};
use gridcraft_model::{AutoFilter, Cell, Workbook};
use serde_json::Value as Json;

/// A hub that notes the size of every submitted update.
pub struct SpyHub {
    inner: MemHub,
    pub sizes: Rc<RefCell<Vec<usize>>>,
}

impl Hub for SpyHub {
    fn send(&mut self, msg: ClientMsg) -> craft_collab::Result<()> {
        if let ClientMsg::Submit { params, .. } = &msg {
            self.sizes.borrow_mut().push(serde_json::to_string(params).map_or(0, |s| s.len()));
        }
        self.inner.send(msg)
    }
    fn poll(&mut self) -> Vec<ServerMsg> {
        self.inner.poll()
    }
    fn connected(&self) -> bool {
        self.inner.connected()
    }
    fn set_token(&mut self, token: String) {
        self.inner.set_token(token);
    }
}

pub struct Peer {
    pub session: Session,
    pub live: GridLive<SpyHub>,
    pub sizes: Rc<RefCell<Vec<usize>>>,
    pub notices: Vec<String>,
}

pub struct Room {
    pub relay: MemRelay,
    pub peers: Vec<Peer>,
    pub bytes: Vec<u8>,
}

pub const FILE: &str = "book.xlsx";

/// The base file: two sheets, constants, a formula, and a column of text for replace tests.
pub fn base_bytes(filtered: bool) -> Vec<u8> {
    let mut wb = Workbook::new();
    if wb.sheets.len() < 2 {
        let name = wb.next_sheet_name();
        wb.sheets.push(std::sync::Arc::new(gridcraft_model::Sheet::new(name)));
    }
    if let Some(s) = wb.sheet_mut(0) {
        for r in 0..8u32 {
            s.set_cell(CellRef::new(r, 0), Cell::value(Value::Number(f64::from(r + 1))));
            s.set_cell(CellRef::new(r, 2), Cell::value(Value::Text(format!("xx {r}").into())));
        }
        s.set_cell(CellRef::new(1, 1), Cell::value(Value::Number(42.0)));
        s.set_cell(CellRef::new(0, 1), Cell::formula(gridcraft_model::Formula::new("=SUM(A1:A8)")));
        if filtered {
            s.autofilter = Some(AutoFilter { range: RangeRef::new(CellRef::new(0, 0), CellRef::new(7, 2)), criteria: Vec::new() });
            s.rows.insert(2, gridcraft_model::LineInfo { hidden: true, ..Default::default() });
        }
    }
    io::save_bytes(&wb, FILE).unwrap()
}

pub fn base_of(bytes: &[u8]) -> Workbook {
    io::open_bytes(FILE, bytes).unwrap().0
}

pub const NAMES: [&str; 8] = ["Alice", "Bob", "Carol", "Dan", "Eve", "Frank", "Grace", "Heidi"];

impl Room {
    pub fn new(n: usize) -> Room {
        Room::with(n, base_bytes(false), None)
    }

    pub fn with(n: usize, bytes: Vec<u8>, hash: Option<StyleHash>) -> Room {
        let mut room = Room { relay: MemRelay::new(), peers: Vec::new(), bytes };
        for name in NAMES.iter().take(n) {
            room.join(name, Access::Edit, hash);
        }
        room.settle();
        room
    }

    pub fn join(&mut self, name: &str, access: Access, hash: Option<StyleHash>) -> usize {
        let uid = name.to_lowercase();
        let hub = self.relay.connect(&uid, name, access == Access::Edit);
        let sizes = Rc::new(RefCell::new(Vec::new()));
        let hub = SpyHub { inner: hub, sizes: sizes.clone() };
        let mut session = Session::new();
        let base = BaseFile { bytes: &self.bytes, name: FILE, expected_sha256: None };
        let live = match hash {
            Some(h) => GridLive::start_with(hub, ROOM, &base, access, &uid, &mut session, h),
            None => GridLive::start(hub, ROOM, &base, access, &uid, &mut session),
        };
        self.peers.push(Peer { session, live: live.unwrap(), sizes, notices: Vec::new() });
        self.peers.len() - 1
    }

    pub fn pump(&mut self, i: usize) {
        let p = &mut self.peers[i];
        p.live.pump(&mut p.session);
        let n = p.live.take_notices();
        p.notices.extend(n);
    }

    /// Pumps every peer, in order, until nothing is left in flight.
    pub fn settle(&mut self) {
        for _ in 0..60 {
            for i in 0..self.peers.len() {
                self.pump(i);
            }
            if self.relay.quiet() {
                for i in 0..self.peers.len() {
                    self.pump(i);
                }
                if self.relay.quiet() {
                    return;
                }
            }
        }
    }

    pub fn run(&mut self, i: usize, id: &str, params: Json) -> Result<Ran, String> {
        let p = &mut self.peers[i];
        p.live.run(&mut p.session, id, params)
    }

    pub fn wb(&self, i: usize) -> &Workbook {
        &self.peers[i].session.active().unwrap().wb
    }

    pub fn base(&self) -> Workbook {
        base_of(&self.bytes)
    }

    pub fn replica_state(&self, i: usize) -> Vec<u8> {
        use craft_collab::Replica;
        self.peers[i].live.replica().unwrap().state()
    }

    /// Everything the peers share about a workbook, as text: values, formulas, styles, lines.
    pub fn finger(&self, i: usize) -> String {
        finger(self.wb(i))
    }

    /// Every peer is still `Live` and none was told the link ended or a change was lost.
    pub fn assert_all_live(&self, what: &str) {
        for (i, p) in self.peers.iter().enumerate() {
            assert!(matches!(p.live.status(), craft_live::LiveStatus::Live { .. }), "{what}: peer {i} is {:?}", p.live.status());
            assert!(
                p.notices.iter().all(|n| !n.contains("Reopen") && !n.contains("could not")),
                "{what}: peer {i} was told of a break: {:?}",
                p.notices
            );
        }
    }

    pub fn notices_of(&self, i: usize) -> &[String] {
        &self.peers[i].notices
    }
}

/// Values, formulas as text, style content and line info of every sheet.
pub fn finger(wb: &Workbook) -> String {
    let mut out = String::new();
    for (i, s) in wb.sheets.iter().enumerate() {
        out.push_str(&format!("sheet {i} w={} h={}\n", s.default_col_width, s.default_row_height));
        let mut cells: Vec<(CellRef, &Cell)> = s.cells.iter().collect();
        cells.sort_by_key(|(c, _)| *c);
        for (c, cell) in cells {
            let f = cell.formula.as_ref().map(|f| f.text.clone());
            let style = serde_json::to_string(wb.styles.get(cell.style)).unwrap();
            out.push_str(&format!("{} {:?} {:?} {}\n", c.a1(), cell.value, f, style));
        }
        // A line with nothing set and no entry are the same line.
        let set = |m: &std::collections::BTreeMap<u32, gridcraft_model::LineInfo>| {
            // Style ids differ between peers (interning order): compare the style itself.
            m.iter()
                .filter(|(_, l)| **l != Default::default())
                .map(|(k, l)| {
                    let st = l.style.map(|id| serde_json::to_string(wb.styles.get(id)).unwrap());
                    (*k, gridcraft_model::LineInfo { style: None, ..*l }, st)
                })
                .collect::<Vec<_>>()
        };
        out.push_str(&format!("rows {:?}\ncols {:?}\n", set(&s.rows), set(&s.cols)));
    }
    out
}

/// A1 helper.
pub fn a1(s: &str) -> CellRef {
    CellRef::parse(s).unwrap()
}

/// The first lines where two fingerprints differ, for a failure message.
pub fn diff(a: &str, b: &str) -> String {
    let (la, lb): (Vec<&str>, Vec<&str>) = (a.lines().collect(), b.lines().collect());
    let n = la.len().max(lb.len());
    let mut out = Vec::new();
    for i in 0..n {
        let (x, y) = (la.get(i).copied().unwrap_or("<none>"), lb.get(i).copied().unwrap_or("<none>"));
        if x != y {
            let cut = |s: &str| s.chars().take(700).collect::<String>();
            out.push(format!("line {i}: {} | {}", cut(x), cut(y)));
            if out.len() >= 4 {
                break;
            }
        }
    }
    out.join("\n")
}
