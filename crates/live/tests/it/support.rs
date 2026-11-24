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

pub use crate::fingerprint::{diff, finger};

/// A hub that notes the size of every submitted update.
pub struct SpyHub {
    inner: MemHub,
    pub sizes: Rc<RefCell<Vec<usize>>>,
    /// For every submit, whether it carried a fence.
    pub fenced: Rc<RefCell<Vec<bool>>>,
    /// For every submit, its `local` counter (what a refusal names).
    pub locals: Rc<RefCell<Vec<u64>>>,
    /// While set, the relay's error answers (a refused submit) are held back.
    pub hold_errors: Rc<RefCell<bool>>,
    held: Vec<ServerMsg>,
    /// While set, the relay's ordered edits (`Op`) reach this peer late (network latency).
    /// Messages ahead of the first held `Op` (a lock answer, say) still arrive; from that `Op`
    /// on, everything is held, in order, as a real link would deliver it late. Nothing is ever
    /// passed ahead of an `Op` the relay ordered before it.
    pub hold_ops: Rc<RefCell<bool>>,
    held_ops: Vec<ServerMsg>,
    /// While set, the next join answer says the room lost its history (`history_lost`, a relay
    /// restart or a room dropped after its grace), as c9-save2's relay does; then it clears.
    pub lose_history: Rc<RefCell<bool>>,
    /// How many lock requests this hub sent.
    pub locks: Rc<RefCell<usize>>,
}

impl SpyHub {
    /// The in-memory hub, for the relay's link controls.
    pub fn mem(&self) -> &MemHub {
        &self.inner
    }
}

impl Hub for SpyHub {
    fn send(&mut self, msg: ClientMsg) -> craft_collab::Result<()> {
        if let ClientMsg::Submit { params, fence, local, .. } = &msg {
            self.sizes.borrow_mut().push(serde_json::to_string(params).map_or(0, |s| s.len()));
            self.fenced.borrow_mut().push(fence.is_some());
            self.locals.borrow_mut().push(*local);
        }
        if matches!(msg, ClientMsg::Lock { .. }) {
            *self.locks.borrow_mut() += 1;
        }
        self.inner.send(msg)
    }
    fn poll(&mut self) -> Vec<ServerMsg> {
        let mut now = self.inner.poll();
        for m in &mut now {
            if let ServerMsg::Joined { history_lost, .. } = m
                && std::mem::take(&mut *self.lose_history.borrow_mut())
            {
                *history_lost = true;
            }
        }
        if *self.hold_ops.borrow() {
            // Hold from the first `Op` on (or everything, when one is already held).
            let from = if self.held_ops.is_empty() { now.iter().position(|m| matches!(m, ServerMsg::Op { .. })).unwrap_or(now.len()) } else { 0 };
            self.held_ops.extend(now.drain(from..));
        } else if !self.held_ops.is_empty() {
            let mut late = std::mem::take(&mut self.held_ops);
            late.append(&mut now);
            now = late;
        }
        if *self.hold_errors.borrow() {
            let (errors, rest): (Vec<_>, Vec<_>) = now.into_iter().partition(|m| matches!(m, ServerMsg::Error { .. }));
            self.held.extend(errors);
            return rest;
        }
        let mut out = std::mem::take(&mut self.held);
        out.append(&mut now);
        out
    }
    fn connected(&self) -> bool {
        self.inner.connected()
    }
    fn set_token(&mut self, token: String) {
        self.inner.set_token(token);
    }
    fn set_space(&mut self, space: u32) {
        self.inner.set_space(space);
    }
}

pub struct Peer {
    pub session: Session,
    pub live: GridLive<SpyHub>,
    pub sizes: Rc<RefCell<Vec<usize>>>,
    pub fenced: Rc<RefCell<Vec<bool>>>,
    pub locals: Rc<RefCell<Vec<u64>>>,
    pub hold_errors: Rc<RefCell<bool>>,
    pub hold_ops: Rc<RefCell<bool>>,
    pub lose_history: Rc<RefCell<bool>>,
    pub locks: Rc<RefCell<usize>>,
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
        let fenced = Rc::new(RefCell::new(Vec::new()));
        let locals = Rc::new(RefCell::new(Vec::new()));
        let hold_errors = Rc::new(RefCell::new(false));
        let hold_ops = Rc::new(RefCell::new(false));
        let locks = Rc::new(RefCell::new(0));
        let lose_history = Rc::new(RefCell::new(false));
        let hub = SpyHub {
            inner: hub,
            sizes: sizes.clone(),
            fenced: fenced.clone(),
            locals: locals.clone(),
            hold_errors: hold_errors.clone(),
            held: Vec::new(),
            hold_ops: hold_ops.clone(),
            held_ops: Vec::new(),
            lose_history: lose_history.clone(),
            locks: locks.clone(),
        };
        let mut session = Session::new();
        let base = BaseFile { bytes: &self.bytes, name: FILE, expected_sha256: None };
        let live = match hash {
            Some(h) => GridLive::start_with(hub, ROOM, &base, access, &uid, &mut session, h),
            None => GridLive::start(hub, ROOM, &base, access, &uid, &mut session),
        };
        self.peers.push(Peer { session, live: live.unwrap(), sizes, fenced, locals, hold_errors, hold_ops, lose_history, locks, notices: Vec::new() });
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

    /// Puts peer `i`'s selection on one cell.
    pub fn select(&mut self, i: usize, cell: &str) {
        let p = &mut self.peers[i];
        let uid = p.session.active().map(|d| d.uid).unwrap();
        p.session.set_selection_quiet(uid, gridcraft_engine::Selection::at(a1(cell))).unwrap();
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

/// A1 helper.
pub fn a1(s: &str) -> CellRef {
    CellRef::parse(s).unwrap()
}
