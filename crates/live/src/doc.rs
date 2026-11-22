//! [`GridDoc`]: the room's yrs document in the `gridcraft/1` schema ([`crate::schema`]).
//!
//! It holds only the changes against the pinned base workbook. Every write is untracked in yrs
//! (Ruling R3): per-user undo is GridCraft's own, so [`Replica::undo`] and [`Replica::redo`]
//! always give `None`.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use craft_collab::replica::{REMOTE, UNTRACKED, integrate, to_any, to_json};
use craft_collab::yrs::types::{Event, PathSegment};
use craft_collab::yrs::{Any, Array, ArrayPrelim, ArrayRef, DeepObservable, Doc, Map, MapPrelim, MapRef, Out, ReadTxn, Transact, TransactionMut};
use craft_collab::{LiveError, Replica, YReplica};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::axis::{Axis, AxisKind, Insertion};
use crate::ids::Id;
use crate::op::Op;
use crate::schema::{
    CELLS, CINFO, COLS, COLS_X, FMT, META, META_BASE, META_EPOCH, META_SEED, OPS, RINFO, ROWS, ROWS_X, SHEET, SHEET_PARTS, SHEETS, STYLES,
    axis_parts, hex, info_part,
};

/// The key the change observers are registered under.
const OBSERVER: &str = "gridcraft-live/changes";

/// A key's state in the overlay.
#[derive(Clone, Debug, PartialEq)]
pub enum Entry<T> {
    /// Not written: the base file's value stands.
    Absent,
    /// Written as `null`: cleared.
    Cleared,
    Set(T),
}

/// One cell's content, latest-wins as a whole. `f` is the formula in id form; `a` the array
/// formula's size; `d` a dynamic array; `py` Python code; `u` the author's uid (Ruling R10).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CellContent {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub v: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub f: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub a: Option<[u32; 2]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub d: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub py: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub u: Option<String>,
    /// The fingerprint of the content the author's replica held at this cell when it wrote this
    /// (see [`CellContent::fingerprint`]; "0" for none; text, because yrs may hand a large integer back as a float): "I replaced that one". A later write that
    /// names a different fingerprint did not see this one, which is how a truly concurrent
    /// overwrite is told from an ordinary later edit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p: Option<String>,
}

impl CellContent {
    /// A hash of the content as stored, naming this exact version of a cell.
    #[must_use]
    pub fn fingerprint(&self) -> String {
        format!("{:016x}", crate::styles::fnv1a64(serde_json::to_string(self).unwrap_or_default().as_bytes()))
    }
}

/// What a room is seeded from: the base bytes' hash and the base workbook's sheet count. Equal
/// seeds give equal [`state`](Replica::state) bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GridSeed {
    pub base_sha256: [u8; 32],
    pub sheets: u32,
}

/// The room's metadata.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Meta {
    /// sha256 of the base bytes, lowercase hex.
    pub base: String,
    /// The RAND seed (Ruling R22 hashes the slot ids with it).
    pub seed: u64,
    /// The recalculation epoch: a count and who bumped it last.
    pub epoch: (u64, String),
}

/// What one remote update changed.
#[derive(Clone, Debug, PartialEq)]
pub enum GridChange {
    /// Op-log entries from index `from` on are new.
    Ops {
        from: u32,
    },
    Cells {
        sheet: Id,
        keys: Vec<String>,
    },
    Formats {
        sheet: Id,
        keys: Vec<String>,
    },
    Lines {
        sheet: Id,
        axis: AxisKind,
        keys: Vec<String>,
    },
    Axis {
        sheet: Id,
        axis: AxisKind,
    },
    SheetProps {
        sheet: Id,
    },
    Styles {
        keys: Vec<String>,
    },
    Meta,
}

/// A change the observers saw, before it is grouped.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Seen {
    /// In a sheet: the part (`None` for the sheet's own props) and the keys.
    Sheet {
        sheet: Id,
        part: Option<String>,
        keys: Vec<String>,
    },
    Styles(Vec<String>),
    Meta,
}

#[derive(Clone)]
struct Roots {
    meta: MapRef,
    sheets: ArrayRef,
    sheet: MapRef,
    styles: MapRef,
    ops: ArrayRef,
}

impl Roots {
    fn of(doc: &Doc) -> Roots {
        Roots {
            meta: doc.get_or_insert_map(META),
            sheets: doc.get_or_insert_array(SHEETS),
            sheet: doc.get_or_insert_map(SHEET),
            styles: doc.get_or_insert_map(STYLES),
            ops: doc.get_or_insert_array(OPS),
        }
    }

    fn sheet<T: ReadTxn>(&self, txn: &T, sheet: Id) -> Option<MapRef> {
        match self.sheet.get(txn, &sheet.text()) {
            Some(Out::YMap(m)) => Some(m),
            _ => None,
        }
    }

    fn part_map<T: ReadTxn>(&self, txn: &T, sheet: Id, part: &str) -> Option<MapRef> {
        match self.sheet(txn, sheet)?.get(txn, part) {
            Some(Out::YMap(m)) => Some(m),
            _ => None,
        }
    }

    fn part_array<T: ReadTxn>(&self, txn: &T, sheet: Id, part: &str) -> Option<ArrayRef> {
        match self.sheet(txn, sheet)?.get(txn, part) {
            Some(Out::YArray(a)) => Some(a),
            _ => None,
        }
    }
}

/// One replica of a room's grid document.
pub struct GridDoc {
    y: YReplica,
    roots: Roots,
    /// What the observers saw, drained by [`Replica::apply`].
    seen: Arc<Mutex<Vec<Seen>>>,
    /// Writes the last [`GridDoc::write`] skipped.
    skipped: usize,
}

impl GridDoc {
    fn assemble(doc: Doc, client: Option<u32>) -> GridDoc {
        let roots = Roots::of(&doc);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        roots.sheet.observe_deep(OBSERVER, move |txn, events| {
            let found: Vec<Seen> = events.iter().filter_map(|e| sheet_change(txn, e)).collect();
            push_seen(&sink, found);
        });
        let sink = Arc::clone(&seen);
        roots.styles.observe_deep(OBSERVER, move |txn, events| {
            let keys: BTreeSet<String> = events.iter().flat_map(|e| event_keys(txn, e)).collect();
            push_seen(&sink, vec![Seen::Styles(keys.into_iter().collect())]);
        });
        let sink = Arc::clone(&seen);
        roots.meta.observe_deep(OBSERVER, move |_, _| push_seen(&sink, vec![Seen::Meta]));
        // No undo scope: every write is untracked (Ruling R3).
        let mut y = YReplica::assemble(doc, client, |_, _| {});
        y.set_untracked(true);
        GridDoc { y, roots, seen, skipped: 0 }
    }

    /// Whether this replica dropped updates it was holding for a dependency that never came
    /// (the replica's own cap, see `YReplica::needs_resync`): it is sound but behind and wants
    /// the room's full state.
    #[must_use]
    pub fn needs_resync(&self) -> bool {
        self.y.needs_resync()
    }

    /// How many writes the last [`write`](Self::write) skipped: writes to a sheet the document
    /// does not hold, to a sheet property that names a part, or of a line id yrs cannot store.
    /// Nonzero means the caller's view now differs from the document; reload that sheet.
    #[must_use]
    pub fn write_skipped(&self) -> usize {
        self.skipped
    }

    fn drain_seen(&self) -> Vec<Seen> {
        // A poisoned lock still holds what the observers saw: take it, never drop it.
        std::mem::take(&mut *self.seen.lock().unwrap_or_else(std::sync::PoisonError::into_inner))
    }

    /// The room's metadata. Missing or unreadable fields read as empty or zero.
    #[must_use]
    pub fn meta(&self) -> Meta {
        let txn = self.y.doc().transact();
        let text = |key: &str| match self.roots.meta.get(&txn, key) {
            Some(Out::Any(Any::String(s))) => s.to_string(),
            _ => String::new(),
        };
        let seed = Id::parse(&text(META_SEED)).map_or(0, |id| id.0);
        let epoch = match self.roots.meta.get(&txn, META_EPOCH) {
            Some(Out::Any(a)) => epoch_of(&to_json(&a)),
            _ => (0, String::new()),
        };
        Meta { base: text(META_BASE), seed, epoch }
    }

    /// Whether the document holds a sheet with this id.
    #[must_use]
    pub fn has_sheet(&self, sheet: Id) -> bool {
        self.roots.sheet(&self.y.doc().transact(), sheet).is_some()
    }

    /// The sheet ids in tab order.
    #[must_use]
    pub fn sheet_ids(&self) -> Vec<Id> {
        let txn = self.y.doc().transact();
        self.roots.sheets.iter(&txn).filter_map(|o| id_of_out(&o)).collect()
    }

    /// One axis of a sheet as stored, showing `limit` positions. A sheet the document does not
    /// hold reads as the base axis.
    #[must_use]
    pub fn axis(&self, sheet: Id, axis: AxisKind, limit: u32) -> Axis {
        let txn = self.y.doc().transact();
        let (ids, gone) = axis_parts(axis);
        let explicit = self.roots.part_array(&txn, sheet, ids).map(|a| a.iter(&txn).filter_map(|o| id_of_out(&o)).collect()).unwrap_or_default();
        let deleted = self.roots.part_map(&txn, sheet, gone).map(|m| m.keys(&txn).filter_map(Id::parse).collect()).unwrap_or_default();
        Axis::from_parts(explicit, deleted, limit)
    }

    fn entry(&self, sheet: Id, part: &str, key: &str) -> Entry<Any> {
        let txn = self.y.doc().transact();
        match self.roots.part_map(&txn, sheet, part).and_then(|m| m.get(&txn, key)) {
            None => Entry::Absent,
            Some(Out::Any(Any::Null | Any::Undefined)) => Entry::Cleared,
            Some(Out::Any(a)) => Entry::Set(a),
            Some(_) => {
                log::warn!("gridcraft-live: {part} holds a nested type; read as the base's");
                Entry::Absent
            }
        }
    }

    /// The cell content at `key`. A value that is not cell content reads as the base's.
    #[must_use]
    pub fn cell(&self, sheet: Id, key: &str) -> Entry<CellContent> {
        match self.entry(sheet, CELLS, key) {
            Entry::Absent => Entry::Absent,
            Entry::Cleared => Entry::Cleared,
            Entry::Set(a) => match serde_json::from_value(to_json(&a)) {
                Ok(c) => Entry::Set(c),
                Err(e) => {
                    log::warn!("gridcraft-live: unreadable cell content ({e}); read as the base's");
                    Entry::Absent
                }
            },
        }
    }

    /// The style key at `key`.
    #[must_use]
    pub fn fmt(&self, sheet: Id, key: &str) -> Entry<String> {
        match self.entry(sheet, FMT, key) {
            Entry::Set(Any::String(s)) => Entry::Set(s.to_string()),
            Entry::Set(_) | Entry::Absent => Entry::Absent,
            Entry::Cleared => Entry::Cleared,
        }
    }

    /// A line property, `key` = `"<id>/<p>"`.
    #[must_use]
    pub fn line(&self, sheet: Id, axis: AxisKind, key: &str) -> Entry<Value> {
        match self.entry(sheet, info_part(axis), key) {
            Entry::Set(a) => Entry::Set(to_json(&a)),
            Entry::Absent => Entry::Absent,
            Entry::Cleared => Entry::Cleared,
        }
    }

    /// A sheet property such as `"defw"`.
    #[must_use]
    pub fn sheet_prop(&self, sheet: Id, name: &str) -> Option<Value> {
        let txn = self.y.doc().transact();
        match self.roots.sheet(&txn, sheet)?.get(&txn, name)? {
            Out::Any(a) => Some(to_json(&a)),
            _ => None,
        }
    }

    /// The canonical style JSON stored under `key`.
    #[must_use]
    pub fn style(&self, key: &str) -> Option<Value> {
        let txn = self.y.doc().transact();
        match self.roots.styles.get(&txn, key)? {
            Out::Any(a) => Some(to_json(&a)),
            _ => None,
        }
    }

    /// How many op-log entries there are.
    #[must_use]
    pub fn ops_len(&self) -> u32 {
        self.roots.ops.len(&self.y.doc().transact())
    }

    /// The op-log entries from index `n` on. An entry that does not read as an op is left out
    /// (and logged); it still counts in [`ops_len`](Self::ops_len).
    #[must_use]
    pub fn ops_from(&self, n: u32) -> Vec<Op> {
        let txn = self.y.doc().transact();
        let skip = usize::try_from(n).unwrap_or(usize::MAX);
        self.roots
            .ops
            .iter(&txn)
            .skip(skip)
            .filter_map(|o| {
                let op = match o {
                    Out::Any(a) => Op::from_json(&to_json(&a)),
                    _ => None,
                };
                if op.is_none() {
                    log::warn!("gridcraft-live: an op-log entry did not read as an op; skipped");
                }
                op
            })
            .collect()
    }

    /// Every key written in the sheet's `"cells"` or `"fmt"`, sorted, each once.
    #[must_use]
    pub fn written_cells(&self, sheet: Id) -> Vec<String> {
        let txn = self.y.doc().transact();
        let mut keys = BTreeSet::new();
        for part in [CELLS, FMT] {
            if let Some(m) = self.roots.part_map(&txn, sheet, part) {
                keys.extend(m.keys(&txn).map(str::to_owned));
            }
        }
        keys.into_iter().collect()
    }

    /// Every key written in the sheet's `"rinfo"` or `"cinfo"` (`"<id>/<p>"`), sorted, each once.
    #[must_use]
    pub fn written_lines(&self, sheet: Id, axis: AxisKind) -> Vec<String> {
        let txn = self.y.doc().transact();
        let mut keys: Vec<String> =
            self.roots.part_map(&txn, sheet, info_part(axis)).map(|m| m.keys(&txn).map(str::to_owned).collect()).unwrap_or_default();
        keys.sort();
        keys
    }

    /// Run `f` as one local transaction and return its update. Never in yrs undo (Ruling R3).
    ///
    /// Refused with [`LiveError::Damaged`] on a damaged replica and [`LiveError::NoClient`]
    /// before a space was granted. A write to a sheet the document does not hold is skipped and
    /// logged (check [`has_sheet`](Self::has_sheet) first); the rest of the transaction is kept,
    /// because a yrs transaction cannot be taken back once it holds writes.
    pub fn write(&mut self, f: impl FnOnce(&mut GridTxn)) -> Result<Vec<u8>, LiveError> {
        self.write_grouped(0, f)
    }

    /// [`write`](Self::write) as one part of command group `group`. Every update of one command
    /// (a large batch is cut into several) is written under the same yrs transaction origin,
    /// `"craft-collab/untracked#cmd<group>"`, so the undo of G11 can take them as one step. Group
    /// 0 is an ungrouped write.
    ///
    /// # Errors
    /// As [`write`](Self::write).
    pub fn write_grouped(&mut self, group: u64, f: impl FnOnce(&mut GridTxn)) -> Result<Vec<u8>, LiveError> {
        if self.y.damaged() {
            return Err(LiveError::Damaged);
        }
        if self.y.client().is_none() {
            return Err(LiveError::NoClient);
        }
        let (update, skipped) = {
            let mut g = GridTxn {
                txn: self.y.doc().transact_mut_with(if group == 0 { UNTRACKED.to_owned() } else { format!("{UNTRACKED}#cmd{group}") }),
                roots: &self.roots,
                skipped: 0,
            };
            f(&mut g);
            (g.txn.encode_update_v1(), g.skipped)
        };
        self.skipped = skipped;
        // This replica's own writes are not remote changes.
        self.drain_seen();
        Ok(update)
    }

    fn changes(&self, ops_before: u32, seen: Vec<Seen>) -> Vec<GridChange> {
        let mut out = Vec::new();
        let ops_after = self.ops_len();
        if ops_after > ops_before {
            out.push(GridChange::Ops { from: ops_before });
        }
        // Grouped by sheet and part, in a fixed order.
        let mut parts: BTreeMap<(u8, Id, String), BTreeSet<String>> = BTreeMap::new();
        let mut styles = BTreeSet::new();
        let mut meta = false;
        for s in seen {
            match s {
                Seen::Sheet { sheet, part, keys } => {
                    let part = part.unwrap_or_default();
                    parts.entry((part_rank(&part), sheet, part)).or_default().extend(keys);
                }
                Seen::Styles(keys) => styles.extend(keys),
                Seen::Meta => meta = true,
            }
        }
        let mut axes = BTreeSet::new();
        for ((_, sheet, part), keys) in parts {
            let keys: Vec<String> = keys.into_iter().collect();
            match part.as_str() {
                ROWS | ROWS_X => {
                    if axes.insert((sheet, 0)) {
                        out.push(GridChange::Axis { sheet, axis: AxisKind::Rows });
                    }
                }
                COLS | COLS_X => {
                    if axes.insert((sheet, 1)) {
                        out.push(GridChange::Axis { sheet, axis: AxisKind::Cols });
                    }
                }
                CELLS => out.push(GridChange::Cells { sheet, keys }),
                FMT => out.push(GridChange::Formats { sheet, keys }),
                RINFO => out.push(GridChange::Lines { sheet, axis: AxisKind::Rows, keys }),
                CINFO => out.push(GridChange::Lines { sheet, axis: AxisKind::Cols, keys }),
                _ => out.push(GridChange::SheetProps { sheet }),
            }
        }
        if !styles.is_empty() {
            out.push(GridChange::Styles { keys: styles.into_iter().collect() });
        }
        if meta {
            out.push(GridChange::Meta);
        }
        out
    }
}

/// The order changes are reported in: axes first, then cells, formats, lines, props.
fn part_rank(part: &str) -> u8 {
    match part {
        ROWS | ROWS_X | COLS | COLS_X => 0,
        CELLS => 1,
        FMT => 2,
        RINFO | CINFO => 3,
        _ => 4,
    }
}

impl Replica for GridDoc {
    type Change = GridChange;
    type Seed = GridSeed;

    /// The seed as client 0, in one transaction, every write in a fixed order: equal seeds give
    /// equal state bytes. Each base sheet gets its map with every M1 part, so no peer ever
    /// creates a nested type concurrently with another.
    fn seeded(seed: &GridSeed) -> Result<GridDoc, LiveError> {
        let doc = YReplica::seed_doc();
        let roots = Roots::of(&doc);
        {
            let mut txn = doc.transact_mut_with(REMOTE);
            let rand = seed.base_sha256.iter().take(8).fold(0u64, |n, b| (n << 8) | u64::from(*b));
            roots.meta.insert(&mut txn, META_BASE, Any::from(hex(&seed.base_sha256)));
            roots.meta.insert(&mut txn, META_EPOCH, epoch_any(0, ""));
            roots.meta.insert(&mut txn, META_SEED, Any::from(Id(rand).text()));
            for i in 0..seed.sheets {
                let sid = Id::base(i);
                roots.sheets.push_back(&mut txn, id_any(sid)?);
                let sheet = roots.sheet.insert(&mut txn, sid.text(), MapPrelim::default());
                for part in SHEET_PARTS {
                    if part == ROWS || part == COLS {
                        sheet.insert(&mut txn, part, ArrayPrelim::default());
                    } else {
                        sheet.insert(&mut txn, part, MapPrelim::default());
                    }
                }
            }
        }
        Ok(GridDoc::assemble(doc, None))
    }

    fn from_state(state: &[u8]) -> Result<GridDoc, LiveError> {
        let doc = YReplica::seed_doc();
        integrate(&doc, state)?;
        Ok(GridDoc::assemble(doc, None))
    }

    /// Space 0 and the space already held are no-ops; any other space rebuilds the document
    /// minting under it, the state carried across.
    fn set_client(&mut self, space: u32) -> Result<(), LiveError> {
        if space == 0 {
            log::warn!("gridcraft-live: client id 0 belongs to the seed and is never adopted");
            return Ok(());
        }
        if self.y.client() == Some(space) {
            return Ok(());
        }
        let doc = self.y.rebuilt(space)?;
        let old = std::mem::replace(self, GridDoc::assemble(doc, Some(space)));
        self.y.carry_marks(&old.y);
        Ok(())
    }

    fn apply(&mut self, update: &[u8]) -> Result<Vec<GridChange>, LiveError> {
        let ops_before = self.ops_len();
        self.drain_seen();
        self.y.apply(update)?;
        let seen = self.drain_seen();
        Ok(self.changes(ops_before, seen))
    }

    fn state(&self) -> Vec<u8> {
        self.y.state()
    }

    fn undo(&mut self) -> Option<Vec<u8>> {
        None
    }

    fn redo(&mut self) -> Option<Vec<u8>> {
        None
    }

    fn damaged(&self) -> bool {
        self.y.damaged()
    }

    fn needs_resync(&self) -> bool {
        self.y.needs_resync()
    }
}

/// One local transaction on a [`GridDoc`]. Writes to a sheet the document does not hold are
/// skipped and logged.
pub struct GridTxn<'a> {
    txn: TransactionMut<'a>,
    roots: &'a Roots,
    /// Writes skipped so far (see [`GridDoc::write_skipped`]).
    skipped: usize,
}

impl GridTxn<'_> {
    /// Ids as yrs numbers. An id past `i64` cannot be stored; it is skipped and counted.
    fn ids_any(&mut self, ids: &[Id]) -> Vec<Any> {
        let out: Vec<Any> = ids.iter().filter_map(|id| id_any(*id).ok()).collect();
        if out.len() < ids.len() {
            self.skipped += ids.len() - out.len();
            log::warn!("gridcraft-live: a line id too large to store was skipped");
        }
        out
    }

    fn part_map(&mut self, sheet: Id, part: &str) -> Option<MapRef> {
        let m = self.roots.part_map(&self.txn, sheet, part);
        if m.is_none() {
            self.skipped += 1;
            log::warn!("gridcraft-live: no sheet {} in the document; a {part} write was skipped", sheet.text());
        }
        m
    }

    fn put(&mut self, sheet: Id, part: &str, key: &str, value: Option<Any>) {
        if let Some(m) = self.part_map(sheet, part) {
            match value {
                Some(v) => {
                    m.insert(&mut self.txn, key, v);
                }
                None => {
                    m.remove(&mut self.txn, key);
                }
            }
        }
    }

    pub fn set_cell(&mut self, sheet: Id, key: &str, c: &CellContent) {
        match serde_json::to_value(c) {
            Ok(v) => self.put(sheet, CELLS, key, Some(to_any(&v))),
            Err(e) => {
                self.skipped += 1;
                log::warn!("gridcraft-live: cell content did not encode ({e}); not written");
            }
        }
    }

    /// Write `null`: the cell is empty, whatever the base holds.
    pub fn clear_cell(&mut self, sheet: Id, key: &str) {
        self.put(sheet, CELLS, key, Some(Any::Null));
    }

    /// Remove the key: the base's cell shows again.
    pub fn reset_cell(&mut self, sheet: Id, key: &str) {
        self.put(sheet, CELLS, key, None);
    }

    pub fn set_fmt(&mut self, sheet: Id, key: &str, style: &str) {
        self.put(sheet, FMT, key, Some(Any::from(style)));
    }

    pub fn clear_fmt(&mut self, sheet: Id, key: &str) {
        self.put(sheet, FMT, key, Some(Any::Null));
    }

    pub fn reset_fmt(&mut self, sheet: Id, key: &str) {
        self.put(sheet, FMT, key, None);
    }

    pub fn set_line(&mut self, sheet: Id, axis: AxisKind, key: &str, v: Value) {
        self.put(sheet, info_part(axis), key, Some(to_any(&v)));
    }

    pub fn reset_line(&mut self, sheet: Id, axis: AxisKind, key: &str) {
        self.put(sheet, info_part(axis), key, None);
    }

    /// Set a sheet property. A name that is one of the sheet's parts (`"cells"`, `"rows"`, …)
    /// is refused and logged, so a property never replaces a part.
    pub fn set_sheet_prop(&mut self, sheet: Id, name: &str, v: Value) {
        if SHEET_PARTS.contains(&name) {
            self.skipped += 1;
            log::warn!("gridcraft-live: sheet property {name} names a part of the sheet; not written");
            return;
        }
        match self.roots.sheet(&self.txn, sheet) {
            Some(m) => {
                m.insert(&mut self.txn, name, to_any(&v));
            }
            None => {
                self.skipped += 1;
                log::warn!("gridcraft-live: no sheet {} in the document; a property write was skipped", sheet.text());
            }
        }
    }

    /// Remove a sheet property: the base file's shows again. A part's name is refused as in
    /// [`set_sheet_prop`](Self::set_sheet_prop).
    pub fn reset_sheet_prop(&mut self, sheet: Id, name: &str) {
        if SHEET_PARTS.contains(&name) {
            self.skipped += 1;
            log::warn!("gridcraft-live: sheet property {name} names a part of the sheet; not removed");
            return;
        }
        match self.roots.sheet(&self.txn, sheet) {
            Some(m) => {
                m.remove(&mut self.txn, name);
            }
            None => {
                self.skipped += 1;
                log::warn!("gridcraft-live: no sheet {} in the document; a property removal was skipped", sheet.text());
            }
        }
    }

    pub fn put_style(&mut self, key: &str, style: Value) {
        self.roots.styles.insert(&mut self.txn, key, to_any(&style));
    }

    /// Write an insert the axis worked out ([`Axis::insert`]): append `ins.materialise`, then
    /// put `ids` right after `ins.prev` (at the front when it is `None`). Ids the array already
    /// holds are left out, as the axis leaves them out.
    pub fn axis_insert(&mut self, sheet: Id, axis: AxisKind, ins: &Insertion, ids: &[Id]) {
        let (part, _) = axis_parts(axis);
        let Some(array) = self.roots.part_array(&self.txn, sheet, part) else {
            self.skipped += 1;
            log::warn!("gridcraft-live: no sheet {} in the document; an axis insert was skipped", sheet.text());
            return;
        };
        let raw: Vec<Option<Id>> = array.iter(&self.txn).map(|o| id_of_out(&o)).collect();
        let mut present: BTreeSet<Id> = raw.iter().flatten().copied().collect();
        let tail: Vec<Id> = ins.materialise.iter().copied().filter(|id| present.insert(*id)).collect();
        let tail = self.ids_any(&tail);
        let len = array.len(&self.txn);
        array.insert_range(&mut self.txn, len, tail);
        let raw: Vec<Option<Id>> = array.iter(&self.txn).map(|o| id_of_out(&o)).collect();
        let at = match ins.prev {
            None => 0,
            Some(prev) => match raw.iter().position(|o| *o == Some(prev)) {
                Some(i) => i + 1,
                None => {
                    log::warn!("gridcraft-live: an axis insert names a line the axis does not hold; appended");
                    raw.len()
                }
            },
        };
        let new: Vec<Id> = ids.iter().copied().filter(|id| present.insert(*id)).collect();
        let new = self.ids_any(&new);
        let at = u32::try_from(at).unwrap_or(u32::MAX).min(array.len(&self.txn));
        array.insert_range(&mut self.txn, at, new);
    }

    pub fn axis_delete(&mut self, sheet: Id, axis: AxisKind, ids: &[Id]) {
        let (_, gone) = axis_parts(axis);
        for id in ids {
            self.put(sheet, gone, &id.text(), Some(Any::Bool(true)));
        }
    }

    pub fn axis_undelete(&mut self, sheet: Id, axis: AxisKind, ids: &[Id]) {
        let (_, gone) = axis_parts(axis);
        for id in ids {
            self.put(sheet, gone, &id.text(), None);
        }
    }

    /// Append an entry to the op log.
    pub fn push_op(&mut self, op: &Op) {
        let len = self.roots.ops.len(&self.txn);
        self.roots.ops.insert_range(&mut self.txn, len, [to_any(&op.to_json())]);
    }

    /// Count the recalculation epoch up by one, naming `by`.
    pub fn bump_epoch(&mut self, by: &str) {
        let n = match self.roots.meta.get(&self.txn, META_EPOCH) {
            Some(Out::Any(a)) => epoch_of(&to_json(&a)).0,
            _ => 0,
        };
        self.roots.meta.insert(&mut self.txn, META_EPOCH, epoch_any(n.saturating_add(1), by));
    }
}

fn push_seen(sink: &Mutex<Vec<Seen>>, found: Vec<Seen>) {
    sink.lock().unwrap_or_else(std::sync::PoisonError::into_inner).extend(found);
}

/// The keys a map event changed (none for other events).
fn event_keys(txn: &TransactionMut, e: &Event) -> Vec<String> {
    match e {
        Event::Map(m) => m.keys(txn).keys().map(|k| k.to_string()).collect(),
        _ => Vec::new(),
    }
}

/// What a nested event under `"sheet"` changed: the sheet from the first path key, the part
/// from the second.
fn sheet_change(txn: &TransactionMut, e: &Event) -> Option<Seen> {
    let path = e.path();
    let Some(PathSegment::Key(sid)) = path.front() else {
        return None;
    };
    let sheet = Id::parse(sid)?;
    let part = match path.get(1) {
        Some(PathSegment::Key(p)) => Some(p.to_string()),
        Some(PathSegment::Index(_)) => return None,
        None => None,
    };
    let keys = if part.as_deref().is_some_and(|p| [CELLS, FMT, RINFO, CINFO].contains(&p)) { event_keys(txn, e) } else { Vec::new() };
    Some(Seen::Sheet { sheet, part, keys })
}

fn id_any(id: Id) -> Result<Any, LiveError> {
    i64::try_from(id.0).map(Any::from).map_err(|_| LiveError::OutOfRange)
}

fn id_of_out(o: &Out) -> Option<Id> {
    match o {
        Out::Any(Any::Number(n)) => n.as_i64().and_then(|i| u64::try_from(i).ok()).map(Id),
        _ => None,
    }
}

fn epoch_any(n: u64, by: &str) -> Any {
    Any::Array(Arc::from([Any::from(Id(n).text()), Any::from(by)]))
}

fn epoch_of(v: &Value) -> (u64, String) {
    let n = v.get(0).and_then(Value::as_str).and_then(Id::parse).map_or(0, |id| id.0);
    let by = v.get(1).and_then(Value::as_str).unwrap_or_default().to_owned();
    (n, by)
}
