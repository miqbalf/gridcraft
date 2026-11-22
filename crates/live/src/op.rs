//! Op-log entries: the ordered edits peers already in the session replay through the engine.
//!
//! In yrs an entry is one JSON object `{id, by, k, s, prev, ids}`. `k` is `"rows.insert"`,
//! `"rows.delete"`, `"rows.undelete"` or the `cols.` forms; `s` is the sheet id; `prev` (inserts
//! only) the array element the new ids follow, or `null` at the front. Ids are base 36 text, the
//! op id too, so a `u64` never meets yrs' `i64` numbers.

use serde_json::{Map, Value, json};

use crate::axis::AxisKind;
use crate::ids::Id;

/// One op-log entry.
#[derive(Clone, Debug, PartialEq)]
pub enum Op {
    Insert { id: u64, by: String, sheet: Id, axis: AxisKind, prev: Option<Id>, ids: Vec<Id> },
    Delete { id: u64, by: String, sheet: Id, axis: AxisKind, ids: Vec<Id> },
    Undelete { id: u64, by: String, sheet: Id, axis: AxisKind, ids: Vec<Id> },
}

impl Op {
    /// The entry as stored in `"ops"`.
    #[must_use]
    pub fn to_json(&self) -> Value {
        let (id, by, sheet, axis, verb, prev, ids) = match self {
            Op::Insert { id, by, sheet, axis, prev, ids } => (id, by, sheet, axis, "insert", Some(prev), ids),
            Op::Delete { id, by, sheet, axis, ids } => (id, by, sheet, axis, "delete", None, ids),
            Op::Undelete { id, by, sheet, axis, ids } => (id, by, sheet, axis, "undelete", None, ids),
        };
        let mut m = Map::new();
        m.insert("id".into(), Value::String(Id(*id).text()));
        m.insert("by".into(), Value::String(by.clone()));
        m.insert("k".into(), Value::String(format!("{}.{verb}", axis_text(*axis))));
        m.insert("s".into(), Value::String(sheet.text()));
        if let Some(prev) = prev {
            m.insert("prev".into(), prev.map_or(Value::Null, |p| Value::String(p.text())));
        }
        m.insert("ids".into(), json!(ids.iter().map(|i| i.text()).collect::<Vec<_>>()));
        Value::Object(m)
    }

    /// An entry read back, or `None` when it is not one [`to_json`](Self::to_json) writes.
    #[must_use]
    pub fn from_json(v: &Value) -> Option<Op> {
        let id = Id::parse(v.get("id")?.as_str()?)?.0;
        let by = v.get("by")?.as_str()?.to_owned();
        let sheet = Id::parse(v.get("s")?.as_str()?)?;
        let (axis, verb) = v.get("k")?.as_str()?.split_once('.')?;
        let axis = match axis {
            "rows" => AxisKind::Rows,
            "cols" => AxisKind::Cols,
            _ => return None,
        };
        let ids = v.get("ids")?.as_array()?.iter().map(|i| i.as_str().and_then(Id::parse)).collect::<Option<Vec<Id>>>()?;
        match verb {
            "insert" => {
                let prev = match v.get("prev")? {
                    Value::Null => None,
                    p => Some(Id::parse(p.as_str()?)?),
                };
                Some(Op::Insert { id, by, sheet, axis, prev, ids })
            }
            "delete" => Some(Op::Delete { id, by, sheet, axis, ids }),
            "undelete" => Some(Op::Undelete { id, by, sheet, axis, ids }),
            _ => None,
        }
    }
}

/// `"rows"` or `"cols"`, as op kinds and the schema's root names spell an axis.
#[must_use]
pub fn axis_text(axis: AxisKind) -> &'static str {
    match axis {
        AxisKind::Rows => "rows",
        AxisKind::Cols => "cols",
    }
}
