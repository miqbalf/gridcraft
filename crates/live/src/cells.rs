//! A cell's shared content: [`CellContent`] to and from an engine [`Cell`], with formulas in id
//! form through `gridcraft_formula::ids`.
//!
//! # The one encoding of a value (`CellContent.v`)
//!
//! | engine value | `v` |
//! |---|---|
//! | number (finite) | a JSON number |
//! | text | a JSON string |
//! | boolean | a JSON boolean |
//! | error | an object `{"e": "<error text>"}`, for example `{"e":"#DIV/0!"}` |
//! | empty | `v` absent (never JSON `null`) |
//!
//! A number that is not finite is shared as `{"e":"#NUM!"}`. A cell with a formula or Python
//! code shares no value: every peer calculates its own, so `v` is absent there. Decoding is the
//! exact inverse for everything an engine cell can hold as a constant; a `v` of any other shape
//! reads as empty.

use std::sync::Arc;

use gridcraft_core::{CellError, CellRef, RangeRef, Value};
use gridcraft_formula::ids::{parse_ids, print_ids};
use gridcraft_model::{Cell, Formula, PyCode, StyleId};
use serde_json::{Map, Value as Json};

use crate::doc::CellContent;
use crate::layout::Layout;

/// The shared form of a constant value; `None` for empty (and for an array, which only a formula
/// result can be, and which is never shared).
#[must_use]
pub fn encode_value(v: &Value) -> Option<Json> {
    match v {
        Value::Empty | Value::Array(_) => None,
        Value::Number(n) => Some(serde_json::Number::from_f64(*n).map_or_else(|| error_json(CellError::Num), Json::Number)),
        Value::Text(t) => Some(Json::String(t.to_string())),
        Value::Bool(b) => Some(Json::Bool(*b)),
        Value::Error(e) => Some(error_json(*e)),
    }
}

fn error_json(e: CellError) -> Json {
    let mut m = Map::new();
    m.insert("e".into(), Json::String(e.as_str().into()));
    Json::Object(m)
}

/// The value a shared `v` stands for; anything unreadable is empty.
#[must_use]
pub fn decode_value(v: Option<&Json>) -> Value {
    match v {
        Some(Json::Number(n)) => n.as_f64().map_or(Value::Empty, Value::number),
        Some(Json::String(s)) => Value::Text(Arc::from(s.as_str())),
        Some(Json::Bool(b)) => Value::Bool(*b),
        Some(Json::Object(o)) => o.get("e").and_then(Json::as_str).and_then(CellError::parse).map_or(Value::Empty, Value::Error),
        _ => Value::Empty,
    }
}

/// Whether two cells hold the same content: a value (formula and Python cells have none of their
/// own), a formula, Python code. Formats are not content.
#[must_use]
pub fn same_content(a: Option<&Cell>, b: Option<&Cell>) -> bool {
    match (a.filter(|c| c.has_content()), b.filter(|c| c.has_content())) {
        (None, None) => true,
        (Some(a), Some(b)) => {
            if a.py.is_some() || b.py.is_some() {
                return a.py.as_deref().map(|p| (&p.code, p.object)) == b.py.as_deref().map(|p| (&p.code, p.object))
                    && a.formula.is_none() == b.formula.is_none();
            }
            match (&a.formula, &b.formula) {
                (None, None) => a.value == b.value,
                (Some(x), Some(y)) => x.text == y.text && x.array == y.array && x.dynamic == y.dynamic,
                _ => false,
            }
        }
        _ => false,
    }
}

/// Where a formula lives: its sheet's id and the layout that prints and reads its references.
pub struct Home<'a> {
    pub layout: &'a Layout,
    /// The sheet's id.
    pub sheet: u64,
}

/// The shared content of `cell`, or `None` when it holds none (a cleared cell). `by` is the
/// author's uid.
#[must_use]
pub fn to_content(cell: &Cell, home: &Home, by: &str) -> Option<CellContent> {
    let u = Some(by.to_owned());
    if let Some(py) = &cell.py {
        return Some(CellContent { py: Some(py.code.clone()), d: py.object.then_some(true), u, ..CellContent::default() });
    }
    if let Some(f) = &cell.formula {
        let text = f.expr_arc().and_then(|e| print_ids(&e, home.sheet, home.layout).ok()).unwrap_or_else(|| f.text.clone());
        let a = f.array.map(|r| [r.end.row - r.start.row + 1, r.end.col - r.start.col + 1]);
        return Some(CellContent { f: Some(text), a, d: f.dynamic.then_some(true), u, ..CellContent::default() });
    }
    encode_value(&cell.value).map(|v| CellContent { v: Some(v), u, ..CellContent::default() })
}

/// An engine cell holding `c`'s content at `at`, with `style`. `None` when it would be blank.
#[must_use]
pub fn from_content(c: Option<&CellContent>, at: CellRef, home: &Home, style: StyleId) -> Option<Cell> {
    let mut cell = Cell { style, ..Cell::default() };
    if let Some(c) = c {
        if let Some(code) = &c.py {
            cell.py = Some(Arc::new(PyCode { code: code.clone(), object: c.d.unwrap_or(false), ..PyCode::default() }));
        } else if let Some(text) = &c.f {
            let mut f = match parse_ids(text, home.sheet, home.layout) {
                Ok(e) => Formula::from_expr(e),
                Err(_) => Formula::new(text),
            };
            f.array = c.a.map(|[rows, cols]| RangeRef::new(at, CellRef::new(at.row + rows.max(1) - 1, at.col + cols.max(1) - 1)));
            f.dynamic = c.d == Some(true);
            cell.formula = Some(Arc::new(f));
        } else {
            cell.value = decode_value(c.v.as_ref());
        }
    }
    (!cell.is_blank()).then_some(cell)
}

/// Whether a formula's text names a random function (so the room's epoch matters).
#[must_use]
pub fn is_random(text: &str) -> bool {
    text.to_ascii_uppercase().contains("RAND")
}
