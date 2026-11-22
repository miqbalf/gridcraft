//! Presence: where each editor's selection is, in slot ids.
//!
//! The payload is `{"s": sheet id, "a": [row id, column id], "r": [[row1, col1, row2, col2], …],
//! "e": 0|1, "o": null}` with every id in base 36 text (see [`Id::text`]). Ids, not positions, so a
//! peer's caret and selection stay on their cells when someone inserts or deletes rows or columns
//! above or left of them: each window resolves the ids through its own [`Layout`] every time the
//! layout moves. A line that was deleted shows at the nearest line that is left (the one that
//! moved into its place, else the one above); an id this window has not heard of yet hides the
//! caret until the id arrives. `o` is reserved (always null); readers ignore it.

use gridcraft_core::{CellRef, RangeRef};
use serde_json::{Value, json};

use crate::axis::Axis;
use crate::ids::Id;
use crate::kind::{GridCaret, GridPos};
use crate::layout::Layout;
use gridcraft_formula::ids::Lookup;

/// The most ranges sent or read; a selection of more is sent as its bounding box.
pub const RANGES_MAX: usize = 32;

/// The payload for a selection on sheet index `sheet`, or `None` when the active cell is not on
/// the layout. Ranges whose corners are off the layout are left out.
#[must_use]
pub fn payload(layout: &Layout, sheet: usize, active: CellRef, ranges: &[RangeRef], editing: bool) -> Option<Value> {
    let s = layout.sheets.get(sheet)?;
    let (ar, ac) = layout.slot_at(sheet, active)?;
    let corners = |r: &RangeRef| -> Option<[String; 4]> {
        let (r1, c1) = layout.slot_at(sheet, r.start)?;
        let (r2, c2) = layout.slot_at(sheet, r.end)?;
        Some([r1.text(), c1.text(), r2.text(), c2.text()])
    };
    // When the box has a corner off the layout, the first ranges are sent instead: the selection is kept.
    let sent: Vec<[String; 4]> = match bounding_box(layout, sheet, ranges) {
        Some(b) if ranges.len() > RANGES_MAX => vec![b],
        _ => ranges.iter().filter_map(corners).take(RANGES_MAX).collect(),
    };
    Some(json!({"s": s.id.text(), "a": [ar.text(), ac.text()], "r": sent, "e": u8::from(editing), "o": null}))
}

/// One range around all of `ranges`.
fn bounding_box(layout: &Layout, sheet: usize, ranges: &[RangeRef]) -> Option<[String; 4]> {
    let mut it = ranges.iter().flat_map(|r| [r.start, r.end]);
    let first = it.next()?;
    let (mut r0, mut r1, mut c0, mut c1) = (first.row, first.row, first.col, first.col);
    for c in it {
        r0 = r0.min(c.row);
        r1 = r1.max(c.row);
        c0 = c0.min(c.col);
        c1 = c1.max(c.col);
    }
    let (ra, ca) = layout.slot_at(sheet, CellRef::new(r0, c0))?;
    let (rb, cb) = layout.slot_at(sheet, CellRef::new(r1, c1))?;
    Some([ra.text(), ca.text(), rb.text(), cb.text()])
}

/// The longest id text: `u64::MAX` in base 36 is 13 digits.
const ID_TEXT_MAX: usize = 13;

fn id_at(v: &Value) -> Option<Id> {
    let t = v.as_str()?;
    if t.len() > ID_TEXT_MAX { None } else { Id::parse(t) }
}

fn corners_of(v: &Value) -> Option<((Id, Id), (Id, Id))> {
    match v.as_array()?.as_slice() {
        [r1, c1, r2, c2] => Some(((id_at(r1)?, id_at(c1)?), (id_at(r2)?, id_at(c2)?))),
        _ => None,
    }
}

/// A caret from its payload, not yet resolved (it is hidden until [`resolve`] runs). `None` when
/// the payload is not one [`payload`] makes: the shape is checked before anything is copied, so
/// an oversized id text or array is refused whole (ids over 13 characters, more than
/// [`RANGES_MAX`] ranges, an `a` that is not two ids, a range that is not four ids).
#[must_use]
pub fn parse(payload: &Value) -> Option<GridCaret> {
    let sheet = id_at(payload.get("s")?)?;
    let active = match payload.get("a")?.as_array()?.as_slice() {
        [r, c] => (id_at(r)?, id_at(c)?),
        _ => return None,
    };
    let ranges = match payload.get("r") {
        None | Some(Value::Null) => Vec::new(),
        Some(r) => {
            let r = r.as_array()?;
            if r.len() > RANGES_MAX {
                return None;
            }
            r.iter().map(corners_of).collect::<Option<Vec<_>>>()?
        }
    };
    let editing = match payload.get("e") {
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_u64() == Some(1),
        _ => false,
    };
    Some(GridCaret { sheet, active, ranges, editing, rev: UNRESOLVED, pos: hidden_pos(editing) })
}

/// The revision of a caret that was never resolved (layout revisions start at 1).
const UNRESOLVED: u64 = 0;

fn hidden_pos(editing: bool) -> GridPos {
    GridPos { sheet: 0, active: CellRef::default(), ranges: Vec::new(), editing, unknown: true }
}

/// Where a line is now: its own position, else its nearest surviving neighbour's; `None` for an
/// id the axis has never heard of.
fn place(axis: &Axis, id: Id) -> Option<u32> {
    match axis.pos_of(id) {
        Lookup::At(p) => Some(p),
        Lookup::Deleted { before, after } => after.or(before),
        Lookup::Unknown => None,
    }
}

/// [`resolve`], skipped when `c` was already resolved under layout revision `rev` (the layout
/// has not moved, so the answer cannot have changed). Returns whether the caret is shown.
pub fn resolve_at(layout: &Layout, rev: u64, c: &mut GridCaret) -> bool {
    if c.rev != rev || rev == UNRESOLVED {
        let shown = resolve(layout, c);
        c.rev = rev;
        return shown;
    }
    !c.pos.unknown
}

/// Works out where `c` is under `layout` and stores it. Returns whether the caret is shown: it is
/// hidden while its sheet or an id of its active cell is unknown here.
pub fn resolve(layout: &Layout, c: &mut GridCaret) -> bool {
    c.pos = hidden_pos(c.editing);
    let Some(i) = layout.sheet_index(c.sheet) else { return false };
    let Some(s) = layout.sheets.get(i) else { return false };
    let cell = |(r, col): (Id, Id)| Some(CellRef::new(place(&s.rows, r)?, place(&s.cols, col)?));
    let Some(active) = cell(c.active) else { return false };
    let ranges = c.ranges.iter().filter_map(|(a, b)| Some(RangeRef::new(cell(*a)?, cell(*b)?))).collect();
    c.pos = GridPos { sheet: i, active, ranges, editing: c.editing, unknown: false };
    true
}
