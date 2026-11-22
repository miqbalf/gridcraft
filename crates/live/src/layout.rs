//! The layout: each sheet's row and column axes, read from the document over the base.
//!
//! In M1 the sheets are the base file's (no sheet is added, removed or renamed live), so a
//! sheet's id is `Id::base(index)` and its name is the base sheet's. A cell's content key is its
//! slot: `cell_key(row id, col id)`.

use gridcraft_core::{CellRef, MAX_COLS, MAX_ROWS};
use gridcraft_model::workbook::Workbook;

use crate::axis::{Axis, AxisKind};
use crate::doc::GridDoc;
use crate::ids::Id;
use gridcraft_formula::ids::{Lookup, Resolver};

/// One sheet's axes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SheetLayout {
    pub id: Id,
    /// The sheet's name, for the formula id form's sheet lookups.
    pub name: String,
    pub rows: Axis,
    pub cols: Axis,
}

impl SheetLayout {
    /// The axis for `axis`, to change.
    pub fn axis_mut(&mut self, axis: AxisKind) -> &mut Axis {
        match axis {
            AxisKind::Rows => &mut self.rows,
            AxisKind::Cols => &mut self.cols,
        }
    }

    /// The axis for `axis`.
    #[must_use]
    pub fn axis(&self, axis: AxisKind) -> &Axis {
        match axis {
            AxisKind::Rows => &self.rows,
            AxisKind::Cols => &self.cols,
        }
    }
}

/// Every sheet's axes, by engine sheet index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Layout {
    pub sheets: Vec<SheetLayout>,
}

impl Layout {
    /// Whether both layouts show the same ids in the same places (see [`Axis::equivalent`]).
    #[must_use]
    pub fn equivalent(&self, other: &Layout) -> bool {
        self.sheets.len() == other.sheets.len()
            && self
                .sheets
                .iter()
                .zip(&other.sheets)
                .all(|(a, b)| a.id == b.id && a.name == b.name && a.rows.equivalent(&b.rows) && a.cols.equivalent(&b.cols))
    }

    /// The layout `doc` gives over `base`.
    #[must_use]
    pub fn from_doc(doc: &GridDoc, base: &Workbook) -> Layout {
        let sheets = base
            .sheets
            .iter()
            .enumerate()
            .filter_map(|(i, sheet)| {
                let id = Id::base(u32::try_from(i).ok()?);
                Some(SheetLayout {
                    id,
                    name: sheet.name.clone(),
                    rows: doc.axis(id, AxisKind::Rows, MAX_ROWS),
                    cols: doc.axis(id, AxisKind::Cols, MAX_COLS),
                })
            })
            .collect();
        Layout { sheets }
    }

    /// The base file's own layout: every position holds its base id.
    #[must_use]
    pub fn of_base(base: &Workbook) -> Layout {
        let sheets = base
            .sheets
            .iter()
            .enumerate()
            .filter_map(|(i, sheet)| {
                Some(SheetLayout {
                    id: Id::base(u32::try_from(i).ok()?),
                    name: sheet.name.clone(),
                    rows: Axis::base(MAX_ROWS),
                    cols: Axis::base(MAX_COLS),
                })
            })
            .collect();
        Layout { sheets }
    }

    /// Whether every axis is the base file's.
    #[must_use]
    pub fn is_base(&self) -> bool {
        self.sheets.iter().all(|s| s.rows.is_base() && s.cols.is_base())
    }

    /// The slot `(row id, col id)` shown at `at` on sheet `sheet`.
    #[must_use]
    pub fn slot_at(&self, sheet: usize, at: CellRef) -> Option<(Id, Id)> {
        let s = self.sheets.get(sheet)?;
        Some((s.rows.id_at(at.row)?, s.cols.id_at(at.col)?))
    }

    /// Where slot `(row, col)` is shown on sheet `sheet`, if both lines are.
    #[must_use]
    pub fn cell_at(&self, sheet: usize, row: Id, col: Id) -> Option<CellRef> {
        let s = self.sheets.get(sheet)?;
        match (s.rows.pos_of(row), s.cols.pos_of(col)) {
            (Lookup::At(r), Lookup::At(c)) => Some(CellRef::new(r, c)),
            _ => None,
        }
    }

    /// The engine index of the sheet with this id.
    #[must_use]
    pub fn sheet_index(&self, id: Id) -> Option<usize> {
        self.sheets.iter().position(|s| s.id == id)
    }

    /// This layout as a resolver for formulas on the sheet with id `host`, where a reference
    /// without a sheet (`None`) means `host`.
    #[must_use]
    pub fn hosted(&self, host: u64) -> Hosted<'_> {
        Hosted { layout: self, host }
    }

    fn sheet_by_id(&self, id: Option<u64>) -> Option<&SheetLayout> {
        self.sheets.iter().find(|s| Some(s.id.0) == id)
    }
}

/// Sheet names match without regard to case, as in a spreadsheet. A reference with no sheet
/// (`None`) resolves to nothing here, because the layout alone does not know the formula's own
/// sheet: use [`Layout::hosted`] for that.
impl Resolver for Layout {
    fn sheet_id(&self, name: &str) -> Option<u64> {
        let lower = name.to_lowercase();
        self.sheets.iter().find(|s| s.name.to_lowercase() == lower).map(|s| s.id.0)
    }

    fn sheet_name(&self, id: u64) -> Option<String> {
        self.sheet_by_id(Some(id)).map(|s| s.name.clone())
    }

    fn line_id(&self, sheet: Option<u64>, axis: AxisKind, pos: u32) -> Option<u64> {
        self.sheet_by_id(sheet)?.axis(axis).id_at(pos).map(|id| id.0)
    }

    fn line_pos(&self, sheet: Option<u64>, axis: AxisKind, id: u64) -> Lookup {
        self.sheet_by_id(sheet).map_or(Lookup::Unknown, |s| s.axis(axis).pos_of(Id(id)))
    }
}

/// A [`Layout`] resolving references without a sheet to one host sheet.
#[derive(Clone, Copy, Debug)]
pub struct Hosted<'a> {
    layout: &'a Layout,
    host: u64,
}

impl Resolver for Hosted<'_> {
    fn sheet_id(&self, name: &str) -> Option<u64> {
        self.layout.sheet_id(name)
    }

    fn sheet_name(&self, id: u64) -> Option<String> {
        self.layout.sheet_name(id)
    }

    fn line_id(&self, sheet: Option<u64>, axis: AxisKind, pos: u32) -> Option<u64> {
        self.layout.line_id(Some(sheet.unwrap_or(self.host)), axis, pos)
    }

    fn line_pos(&self, sheet: Option<u64>, axis: AxisKind, id: u64) -> Lookup {
        self.layout.line_pos(Some(sheet.unwrap_or(self.host)), axis, id)
    }
}
