//! Per-user undo and redo (G11).
//!
//! Undo is never a rewind of the room: it is a new forward edit of this editor's own change
//! only. Each command this window wrote is one [`Step`], built from the same group its updates
//! went out under (see [`crate::doc::GridDoc::write_grouped`]), however many updates it took:
//!
//! - a free edit keeps, per key it wrote, what the key held before and what it held right after
//!   ([`KeyWrite`]). Its undo writes `before` back only where the key still holds `after`; a key
//!   someone else changed since is left as it is and named in one notice;
//! - an ordered edit (rows or columns) keeps its op and the op that takes it back. Its undo is a
//!   new op through the structure lock like any ordered edit, refused when a later op by someone
//!   else touched the same lines.
//!
//! The log follows the command groups, not `craft-live`'s own undo counts: every write is
//! untracked in yrs (Ruling R3), so the replica has no undo of its own to count, and only the
//! mapping knows which keys a command wrote and how to take an ordered edit back.

use serde_json::Value;

use crate::axis::AxisKind;
use crate::doc::{CellContent, Entry, GridChange, GridDoc, GridTxn};
use crate::ids::Id;
use crate::op::Op;

/// The most steps kept on each stack; the oldest go first.
pub const UNDO_MAX: usize = 100;
/// The most memory each stack may hold, estimated (see [`step_bytes`]); the oldest steps go
/// first, but the newest step is always kept. Both stacks together stay near 96 MiB, which a
/// 32-bit wasm page (at most 4 GiB of memory) can spare next to the workbook; a 100,000-cell
/// paste is about 33 MiB by this estimate, so one fits with room for smaller steps.
pub const UNDO_BYTES_MAX: usize = 48 * 1024 * 1024;
/// Told when older steps had to go to keep the history within [`UNDO_BYTES_MAX`].
pub const UNDO_TRIMMED_TEXT: &str = "The undo history is full: your oldest changes can no longer be undone.";
/// Told when the replica was replaced (a lost fenced run taken back, or the room's checkpoint) and
/// this window's undo history had to go with it.
pub const UNDO_RESET_TEXT: &str = "Your undo history was cleared because the sheet was reloaded from the room.";
/// Told when a change cannot be undone as one step, so the history before it was cleared.
pub const UNDO_CLEARED_TEXT: &str = "That change cannot be undone, so your earlier changes can no longer be undone either.";

/// Where a key lives in a sheet's map.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Root {
    Cells,
    Fmt,
    Line(AxisKind),
    /// A sheet property; the key is its name.
    Prop,
}

/// One key a command wrote: what it held before, and what it held right after.
#[derive(Clone, Debug, PartialEq)]
pub struct KeyWrite {
    pub root: Root,
    pub sheet: Id,
    pub key: String,
    pub before: Entry<Value>,
    /// Kept as an [`Entry`] (not a bare value) so "removed" and "written as null" stay apart.
    pub after: Entry<Value>,
}

/// One undo step: everything one command did.
#[derive(Clone, Debug, PartialEq)]
pub enum Step {
    Free {
        writes: Vec<KeyWrite>,
    },
    /// A row or column edit. `op` is the last op the command wrote (ops after it in the log are
    /// later ones), `inverse` the op that takes it back (its id is minted when it runs), `region`
    /// the sheet, the axis and every line the command named. `writes` are the free writes that
    /// went with it (a row's size, a formula a delete rewrote), taken back after the op.
    Ordered {
        op: Box<Op>,
        inverse: Box<Op>,
        region: (Id, AxisKind, Vec<Id>),
        writes: Vec<KeyWrite>,
    },
}

/// This window's undo and redo stacks, each with the estimated bytes of its steps.
#[derive(Clone, Debug, Default)]
pub struct UndoLog {
    pub undo: Vec<Step>,
    pub redo: Vec<Step>,
    undo_bytes: Vec<usize>,
    redo_bytes: Vec<usize>,
    /// Steps were dropped for the byte budget since the last [`take_trimmed`](Self::take_trimmed).
    trimmed: bool,
}

impl UndoLog {
    /// A new local command: on the undo stack, and redo is cleared.
    pub fn record(&mut self, step: Step) {
        self.redo.clear();
        self.redo_bytes.clear();
        self.push(false, step);
    }

    /// The step a replayed undo (or redo) wrote, onto the other stack.
    pub fn record_replay(&mut self, step: Step, redo: bool) {
        self.push(!redo, step);
    }

    /// Takes the next step to undo (or redo).
    pub fn take(&mut self, redo: bool) -> Option<Step> {
        if redo {
            self.redo_bytes.pop();
            self.redo.pop()
        } else {
            self.undo_bytes.pop();
            self.undo.pop()
        }
    }

    /// Puts a step back where [`take`](Self::take) found it.
    pub fn put_back(&mut self, step: Step, redo: bool) {
        self.push(redo, step);
    }

    /// Whether neither stack holds a step.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.undo.is_empty() && self.redo.is_empty()
    }

    /// Whether steps were dropped for the byte budget since the last call.
    pub fn take_trimmed(&mut self) -> bool {
        std::mem::take(&mut self.trimmed)
    }

    /// The estimated bytes both stacks hold.
    #[cfg(test)]
    #[must_use]
    pub fn bytes(&self) -> usize {
        self.undo_bytes.iter().chain(&self.redo_bytes).sum()
    }

    /// Every step on both stacks.
    pub(crate) fn steps_mut(&mut self) -> impl Iterator<Item = &mut Step> {
        self.undo.iter_mut().chain(self.redo.iter_mut())
    }

    fn push(&mut self, redo: bool, step: Step) {
        let bytes = step_bytes(&step);
        let (stack, sizes) = if redo { (&mut self.redo, &mut self.redo_bytes) } else { (&mut self.undo, &mut self.undo_bytes) };
        stack.push(step);
        sizes.push(bytes);
        let mut total: usize = sizes.iter().sum();
        let mut drop = 0;
        while stack.len() - drop > 1 && (stack.len() - drop > UNDO_MAX || total > UNDO_BYTES_MAX) {
            if stack.len() - drop <= UNDO_MAX {
                self.trimmed = true;
            }
            total -= sizes.get(drop).copied().unwrap_or(0);
            drop += 1;
        }
        stack.drain(..drop);
        sizes.drain(..drop);
    }
}

/// A rough size in memory of a JSON value.
fn value_bytes(v: &Value) -> usize {
    match v {
        Value::Null | Value::Bool(_) | Value::Number(_) => 16,
        Value::String(s) => 24 + s.len(),
        Value::Array(a) => 24 + a.iter().map(value_bytes).sum::<usize>(),
        Value::Object(o) => 48 + o.iter().map(|(k, v)| 32 + k.len() + value_bytes(v)).sum::<usize>(),
    }
}

fn entry_bytes(e: &Entry<Value>) -> usize {
    match e {
        Entry::Set(v) => value_bytes(v),
        _ => 0,
    }
}

/// The estimated memory a step holds.
#[must_use]
pub fn step_bytes(step: &Step) -> usize {
    let writes = match step {
        Step::Free { writes } => writes,
        Step::Ordered { region, writes, .. } => {
            return 256 + region.2.len() * 8 + writes.iter().map(write_bytes).sum::<usize>();
        }
    };
    64 + writes.iter().map(write_bytes).sum::<usize>()
}

fn write_bytes(w: &KeyWrite) -> usize {
    std::mem::size_of::<KeyWrite>() + w.key.len() + entry_bytes(&w.before) + entry_bytes(&w.after)
}

/// A key's identity across steps: the root's rank, the sheet and the key.
type KeyId = (u8, Id, String);
/// What an undo meant to write at a key, and what it wrote.
type Rewrite<'a> = (&'a Entry<Value>, &'a Entry<Value>);

/// A key's identity across steps.
fn key_of(w: &KeyWrite) -> (u8, Id, &str) {
    let rank = match w.root {
        Root::Cells => 0,
        Root::Fmt => 1,
        Root::Line(AxisKind::Rows) => 2,
        Root::Line(AxisKind::Cols) => 3,
        Root::Prop => 4,
    };
    (rank, w.sheet, w.key.as_str())
}

/// One write per key: the first `before` and the last `after` (a command whose parts wrote one key
/// twice, such as an ordered undo's own revert, takes it back in one go).
#[must_use]
pub fn merged(writes: Vec<KeyWrite>) -> Vec<KeyWrite> {
    let mut at: std::collections::HashMap<KeyId, usize> = std::collections::HashMap::new();
    let mut out: Vec<KeyWrite> = Vec::with_capacity(writes.len());
    for w in writes {
        let (r, s, k) = key_of(&w);
        match at.get(&(r, s, k.to_owned())) {
            Some(i) => {
                if let Some(first) = out.get_mut(*i) {
                    first.after = w.after;
                }
            }
            None => {
                at.insert((r, s, k.to_owned()), out.len());
                out.push(w);
            }
        }
    }
    // A key back where it started is no change at all.
    out.retain(|w| w.before != w.after);
    out
}

/// After an undo wrote `written` where it meant `meant` (a cell is re-signed by the undoer), the
/// older steps that left the key at `meant` now find it at `written`: their `after` follows, so
/// this editor's own undos chain and are never taken for someone else's change.
pub(crate) fn follow_rewrites(log: &mut UndoLog, rewrites: &[(KeyWrite, Entry<Value>)]) {
    let changed: std::collections::HashMap<KeyId, Rewrite<'_>> = rewrites
        .iter()
        .filter(|(w, written)| w.before != *written)
        .map(|(w, written)| {
            let (r, s, k) = key_of(w);
            ((r, s, k.to_owned()), (&w.before, written))
        })
        .collect();
    if changed.is_empty() {
        return;
    }
    for step in log.steps_mut() {
        let writes = match step {
            Step::Free { writes } | Step::Ordered { writes, .. } => writes,
        };
        for w in writes.iter_mut() {
            let (r, s, k) = key_of(w);
            if let Some((meant, written)) = changed.get(&(r, s, k.to_owned()))
                && w.after == **meant
            {
                w.after = (*written).clone();
            }
        }
    }
}

/// An undo or redo waiting for the structure lock, or running.
#[derive(Clone, Debug)]
pub(crate) struct Replay {
    /// A redo (the step came off the redo stack).
    pub redo: bool,
    pub step: Step,
}

/// The ordered step of a command's ops, or `None` when there are none. `Err` when the ops cannot
/// be taken back as one (different sheets, axes or kinds in one command).
pub(crate) fn ordered_step(ops: &[Op], uid: &str, writes: Vec<KeyWrite>) -> Result<Option<Step>, ()> {
    let Some(last) = ops.last() else { return Ok(None) };
    let (sheet, axis) = parts(last).0;
    let kind = kind_of(last);
    let mut ids: Vec<Id> = Vec::new();
    for op in ops {
        let ((s, a), op_ids) = parts(op);
        if s != sheet || a != axis || kind_of(op) != kind {
            return Err(());
        }
        ids.extend(op_ids.iter().copied());
    }
    let by = uid.to_owned();
    let inverse = match last {
        Op::Insert { .. } | Op::Undelete { .. } => Op::Delete { id: 0, by, sheet, axis, ids: ids.clone() },
        Op::Delete { .. } => Op::Undelete { id: 0, by, sheet, axis, ids: ids.clone() },
    };
    Ok(Some(Step::Ordered { op: Box::new(last.clone()), inverse: Box::new(inverse), region: (sheet, axis, ids), writes }))
}

fn parts(op: &Op) -> ((Id, AxisKind), &[Id]) {
    match op {
        Op::Insert { sheet, axis, ids, .. } | Op::Delete { sheet, axis, ids, .. } | Op::Undelete { sheet, axis, ids, .. } => ((*sheet, *axis), ids),
    }
}

fn kind_of(op: &Op) -> u8 {
    match op {
        Op::Insert { .. } => 0,
        Op::Delete { .. } => 1,
        Op::Undelete { .. } => 2,
    }
}

fn op_id(op: &Op) -> u64 {
    match op {
        Op::Insert { id, .. } | Op::Delete { id, .. } | Op::Undelete { id, .. } => *id,
    }
}

/// Whether someone else built on an ordered step since.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Touched {
    No,
    /// A later op by this uid named one of the step's lines (or, for a delete, put new lines
    /// right after one of them, where the deleted lines were).
    By(String),
    /// This editor's op is not in the log (the room's state was replaced): nothing can be said.
    Unknown,
}

/// Who, if anyone, touched `region` with an op later in `log` than `mine`. Inserting after one
/// of the lines counts only when the step is a delete: the undo of an insert takes only its own
/// lines away, and rows someone put after them stay.
#[must_use]
pub fn touched_by(log: &[Op], mine: &Op, region: &(Id, AxisKind, Vec<Id>), me: &str) -> Touched {
    let Some(at) = log.iter().position(|o| op_id(o) == op_id(mine) && kind_of(o) == kind_of(mine)) else { return Touched::Unknown };
    let (sheet, axis, ids) = region;
    let set: std::collections::BTreeSet<Id> = ids.iter().copied().collect();
    let deleted = matches!(mine, Op::Delete { .. });
    for later in log.iter().skip(at + 1) {
        let (by, ((s, a), op_ids)) = match later {
            Op::Insert { by, .. } | Op::Delete { by, .. } | Op::Undelete { by, .. } => (by, parts(later)),
        };
        if by == me || s != *sheet || a != *axis {
            continue;
        }
        let built_on = deleted && matches!(later, Op::Insert { prev: Some(p), .. } if set.contains(p));
        if built_on || op_ids.iter().any(|i| set.contains(i)) {
            return Touched::By(by.clone());
        }
    }
    Touched::No
}

/// What a key holds now.
#[must_use]
pub fn read(doc: &GridDoc, root: Root, sheet: Id, key: &str) -> Entry<Value> {
    match root {
        Root::Cells => match doc.cell(sheet, key) {
            Entry::Set(c) => serde_json::to_value(c).map_or(Entry::Absent, Entry::Set),
            Entry::Cleared => Entry::Cleared,
            Entry::Absent => Entry::Absent,
        },
        Root::Fmt => match doc.fmt(sheet, key) {
            Entry::Set(s) => Entry::Set(Value::String(s)),
            Entry::Cleared => Entry::Cleared,
            Entry::Absent => Entry::Absent,
        },
        Root::Line(axis) => doc.line(sheet, axis, key),
        Root::Prop => match doc.sheet_prop(sheet, key) {
            Some(Value::Null) => Entry::Cleared,
            Some(v) => Entry::Set(v),
            None => Entry::Absent,
        },
    }
}

/// Writes `e` at the key.
pub(crate) fn write(t: &mut GridTxn, root: Root, sheet: Id, key: &str, e: &Entry<Value>) {
    match (root, e) {
        (Root::Cells, Entry::Set(v)) => match serde_json::from_value::<CellContent>(v.clone()) {
            Ok(c) => t.set_cell(sheet, key, &c),
            Err(_) => t.clear_cell(sheet, key),
        },
        (Root::Cells, Entry::Cleared) => t.clear_cell(sheet, key),
        (Root::Cells, Entry::Absent) => t.reset_cell(sheet, key),
        (Root::Fmt, Entry::Set(Value::String(s))) => t.set_fmt(sheet, key, s),
        (Root::Fmt, Entry::Set(_) | Entry::Cleared) => t.clear_fmt(sheet, key),
        (Root::Fmt, Entry::Absent) => t.reset_fmt(sheet, key),
        (Root::Line(axis), Entry::Set(v)) => t.set_line(sheet, axis, key, v.clone()),
        (Root::Line(axis), Entry::Cleared) => t.set_line(sheet, axis, key, Value::Null),
        (Root::Line(axis), Entry::Absent) => t.reset_line(sheet, axis, key),
        (Root::Prop, Entry::Set(v)) => t.set_sheet_prop(sheet, key, v.clone()),
        (Root::Prop, Entry::Cleared) => t.set_sheet_prop(sheet, key, Value::Null),
        (Root::Prop, Entry::Absent) => t.reset_sheet_prop(sheet, key),
    }
}

/// A key's estimated share of an update, in bytes (as [`crate::split`] counts).
pub(crate) fn cost(w: &KeyWrite) -> usize {
    let v = match &w.before {
        Entry::Set(v) => v.to_string().len() * 2,
        _ => 0,
    };
    96 + w.key.len() + v
}

/// The changes a set of written keys makes, as a remote update would report them.
pub(crate) fn changes_of(keys: &[(Root, Id, String)]) -> Vec<GridChange> {
    use std::collections::BTreeMap;
    let rank = |r: Root| match r {
        Root::Cells => 0u8,
        Root::Fmt => 1,
        Root::Line(AxisKind::Rows) => 2,
        Root::Line(AxisKind::Cols) => 3,
        Root::Prop => 4,
    };
    let mut by: BTreeMap<(u8, Id), (Root, Vec<String>)> = BTreeMap::new();
    for (root, sheet, key) in keys {
        by.entry((rank(*root), *sheet)).or_insert_with(|| (*root, Vec::new())).1.push(key.clone());
    }
    by.into_iter()
        .map(|((_, sheet), (root, keys))| match root {
            Root::Cells => GridChange::Cells { sheet, keys },
            Root::Fmt => GridChange::Formats { sheet, keys },
            Root::Line(axis) => GridChange::Lines { sheet, axis, keys },
            Root::Prop => GridChange::SheetProps { sheet },
        })
        .collect()
}

/// The text the user is told for keys an undo left alone; `name` gives a person's display name
/// for a uid, `place` a key's A1 name.
pub(crate) fn skipped_text(skipped: &[(Option<String>, String)]) -> String {
    const SHOWN: usize = 10;
    let mut parts: Vec<String> = skipped
        .iter()
        .take(SHOWN)
        .map(|(who, at)| match who {
            Some(who) => format!("{who} changed {at} since; not undone"),
            None => format!("{at} was changed by someone else since; not undone"),
        })
        .collect();
    if skipped.len() > SHOWN {
        parts.push(format!("{} more places were changed by someone else since; not undone", skipped.len() - SHOWN));
    }
    let mut text = parts.join(". ");
    text.push('.');
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn big(n: usize) -> Step {
        let w = |i: usize| KeyWrite {
            root: Root::Cells,
            sheet: Id::base(0),
            key: format!("k{i}"),
            before: Entry::Absent,
            after: Entry::Set(Value::String("x".repeat(4000))),
        };
        Step::Free { writes: (0..n).map(w).collect() }
    }

    #[test]
    fn the_stacks_keep_within_the_byte_budget_and_always_the_newest_step() {
        let mut log = UndoLog::default();
        let one = step_bytes(&big(7_000));
        assert!(one * 2 > UNDO_BYTES_MAX && one < UNDO_BYTES_MAX, "the test step is over half the budget: {one}");
        log.record(big(7_000));
        assert!(!log.take_trimmed());
        log.record(big(7_000));
        assert!(log.take_trimmed(), "the oldest went, and that is told");
        assert_eq!(log.undo.len(), 1);
        assert!(log.bytes() <= UNDO_BYTES_MAX);
        let huge = big(15_000);
        assert!(step_bytes(&huge) > UNDO_BYTES_MAX);
        log.record(huge);
        assert_eq!(log.undo.len(), 1, "a step over the budget is still kept, alone");
    }
}
