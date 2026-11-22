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

/// This window's undo and redo stacks.
#[derive(Clone, Debug, Default)]
pub struct UndoLog {
    pub undo: Vec<Step>,
    pub redo: Vec<Step>,
}

impl UndoLog {
    /// A new local command: on the undo stack, and redo is cleared.
    pub fn record(&mut self, step: Step) {
        self.redo.clear();
        push(&mut self.undo, step);
    }

    /// The step a replayed undo (or redo) wrote, onto the other stack.
    pub fn record_replay(&mut self, step: Step, redo: bool) {
        if redo {
            push(&mut self.undo, step);
        } else {
            push(&mut self.redo, step);
        }
    }

    /// Takes the next step to undo (or redo).
    pub fn take(&mut self, redo: bool) -> Option<Step> {
        if redo { self.redo.pop() } else { self.undo.pop() }
    }

    /// Puts a step back where [`take`](Self::take) found it.
    pub fn put_back(&mut self, step: Step, redo: bool) {
        push(if redo { &mut self.redo } else { &mut self.undo }, step);
    }
}

fn push(stack: &mut Vec<Step>, step: Step) {
    stack.push(step);
    if stack.len() > UNDO_MAX {
        let extra = stack.len() - UNDO_MAX;
        stack.drain(..extra);
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

/// The uid of someone else whose op, later in `log` than `mine`, touched `region`: named one of
/// its lines, or put new lines right after one of them (built on it). `None` when nobody did.
#[must_use]
pub fn touched_by(log: &[Op], mine: &Op, region: &(Id, AxisKind, Vec<Id>), me: &str) -> Option<String> {
    let at = log.iter().position(|o| op_id(o) == op_id(mine) && kind_of(o) == kind_of(mine))?;
    let (sheet, axis, ids) = region;
    let set: std::collections::BTreeSet<Id> = ids.iter().copied().collect();
    for later in log.iter().skip(at + 1) {
        let (by, ((s, a), op_ids)) = match later {
            Op::Insert { by, .. } | Op::Delete { by, .. } | Op::Undelete { by, .. } => (by, parts(later)),
        };
        if by == me || s != *sheet || a != *axis {
            continue;
        }
        let built_on = matches!(later, Op::Insert { prev: Some(p), .. } if set.contains(p));
        if built_on || op_ids.iter().any(|i| set.contains(i)) {
            return Some(by.clone());
        }
    }
    None
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
        parts.push(format!("{} more cells were changed by someone else since; not undone", skipped.len() - SHOWN));
    }
    let mut text = parts.join(". ");
    text.push('.');
    text
}
