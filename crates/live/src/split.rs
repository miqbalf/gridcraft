//! A batch of local edits as a [`Plan`] of writes, and its split into updates.
//!
//! One batch becomes updates of at most [`UPDATE_MAX_BYTES`] (the relay's cap), each a complete
//! set of cells: a cell's content, its style key and the style itself always travel in the same
//! update, so a receiver never sees half a cell. Lines, sheet properties and the epoch bump go
//! with the first update. Sizes are estimated from the content (twice its JSON length plus a
//! fixed cost per key), against a budget of [`CHUNK_BUDGET_BYTES`], which leaves room for yrs's
//! framing; a test checks that real updates stay under the cap.

use std::collections::BTreeMap;

use serde_json::Value as Json;

use crate::axis::AxisKind;
use crate::doc::{CellContent, GridTxn};
use crate::ids::Id;

/// The largest update the relay takes.
pub const UPDATE_MAX_BYTES: usize = 1024 * 1024;
/// What one update's estimated size may reach.
pub const CHUNK_BUDGET_BYTES: usize = 640 * 1024;
/// Estimated bytes of a key and its framing.
const KEY_COST: usize = 48;

/// What to do with one key of the overlay.
#[derive(Clone, Debug, PartialEq)]
pub enum Put<T> {
    /// Leave it as it is (another peer may have written it).
    Keep,
    /// Remove the key: back to the base file's.
    Reset,
    /// Write `null`: cleared.
    Clear,
    Set(T),
}

impl<T> Put<T> {
    fn merged(self, later: Put<T>) -> Put<T> {
        if matches!(later, Put::Keep) { self } else { later }
    }
}

/// Everything to write for one cell.
#[derive(Clone, Debug, PartialEq)]
pub struct CellWrite {
    pub sheet: Id,
    pub key: String,
    pub content: Put<CellContent>,
    pub fmt: Put<String>,
    /// The style `fmt` names: its key and canonical JSON, written with it.
    pub style: Option<(String, Json)>,
}

impl CellWrite {
    fn size(&self) -> usize {
        let content = match &self.content {
            Put::Set(c) => serde_json::to_string(c).map_or(256, |s| s.len() * 2),
            _ => 0,
        };
        let style = self.style.as_ref().map_or(0, |(k, j)| k.len() + j.to_string().len() * 2);
        KEY_COST * 2 + self.key.len() + content + style
    }
}

/// The writes of one batch, later edits over earlier ones.
#[derive(Debug, Default)]
pub struct Plan {
    cells: BTreeMap<(Id, String), CellWrite>,
    lines: BTreeMap<(Id, u8, String), (AxisKind, Put<Json>)>,
    props: BTreeMap<(Id, &'static str), Json>,
    /// Bump the room's recalculation epoch.
    pub epoch: bool,
}

impl Plan {
    /// Adds a cell's writes; parts that are [`Put::Keep`] leave the earlier ones.
    pub fn cell(&mut self, w: CellWrite) {
        let k = (w.sheet, w.key.clone());
        match self.cells.remove(&k) {
            Some(old) => {
                let style = w.style.clone().or(old.style.filter(|_| matches!(w.fmt, Put::Keep)));
                self.cells.insert(k, CellWrite { content: old.content.merged(w.content), fmt: old.fmt.merged(w.fmt), style, ..w });
            }
            None => {
                self.cells.insert(k, w);
            }
        }
    }

    pub fn line(&mut self, sheet: Id, axis: AxisKind, key: String, put: Put<Json>) {
        let k = (sheet, u8::from(axis == AxisKind::Cols), key);
        let merged = match self.lines.remove(&k) {
            Some((_, old)) => old.merged(put),
            None => put,
        };
        self.lines.insert(k, (axis, merged));
    }

    pub fn prop(&mut self, sheet: Id, name: &'static str, value: Json) {
        self.props.insert((sheet, name), value);
    }

    /// Whether nothing is to be written.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.cells.is_empty() && self.lines.is_empty() && self.props.is_empty() && !self.epoch
    }

    /// The cells written, as `(sheet, key)`.
    pub fn cell_keys(&self) -> impl Iterator<Item = &(Id, String)> {
        self.cells.keys()
    }

    /// Cuts the plan into updates' worth of writes, each within [`CHUNK_BUDGET_BYTES`].
    #[must_use]
    pub fn split(self) -> Vec<Chunk> {
        let mut chunks = vec![Chunk {
            cells: Vec::new(),
            lines: self.lines.into_iter().map(|((sheet, _, key), (axis, put))| ((sheet, axis, key), put)).collect(),
            props: self.props.into_iter().collect(),
            epoch: self.epoch,
        }];
        let mut used = 0usize;
        for (_, w) in self.cells {
            let size = w.size();
            if used > 0 && used + size > CHUNK_BUDGET_BYTES {
                chunks.push(Chunk { cells: Vec::new(), lines: Vec::new(), props: Vec::new(), epoch: false });
                used = 0;
            }
            used += size;
            if let Some(c) = chunks.last_mut() {
                c.cells.push(w);
            }
        }
        chunks
    }
}

/// The writes of one update.
#[derive(Debug)]
pub struct Chunk {
    pub cells: Vec<CellWrite>,
    pub lines: Vec<((Id, AxisKind, String), Put<Json>)>,
    pub props: Vec<((Id, &'static str), Json)>,
    pub epoch: bool,
}

impl Chunk {
    /// Writes everything into one transaction. `by` is the author's uid.
    pub fn write(&self, t: &mut GridTxn, by: &str) {
        let mut put_styles = std::collections::BTreeSet::new();
        for w in &self.cells {
            match &w.content {
                Put::Keep => {}
                Put::Reset => t.reset_cell(w.sheet, &w.key),
                Put::Clear => t.clear_cell(w.sheet, &w.key),
                Put::Set(c) => t.set_cell(w.sheet, &w.key, c),
            }
            if let Some((k, j)) = &w.style
                && put_styles.insert(k.clone())
            {
                t.put_style(k, j.clone());
            }
            match &w.fmt {
                Put::Keep => {}
                Put::Reset => t.reset_fmt(w.sheet, &w.key),
                Put::Clear => t.clear_fmt(w.sheet, &w.key),
                Put::Set(k) => t.set_fmt(w.sheet, &w.key, k),
            }
        }
        for ((sheet, axis, key), put) in &self.lines {
            match put {
                Put::Keep => {}
                Put::Reset => t.reset_line(*sheet, *axis, key),
                Put::Clear => t.set_line(*sheet, *axis, key, Json::Null),
                Put::Set(v) => t.set_line(*sheet, *axis, key, v.clone()),
            }
        }
        for ((sheet, name), v) in &self.props {
            t.set_sheet_prop(*sheet, name, v.clone());
        }
        if self.epoch {
            t.bump_epoch(by);
        }
    }
}
