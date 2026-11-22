//! The types of a GridCraft live session on top of `craft-live`: what a local edit is, and what a
//! caret is.

use craft_live::LiveKind;
use gridcraft_core::{CellRef, RangeRef};
use gridcraft_engine::EditReport;

use crate::doc::GridDoc;
use crate::ids::Id;

/// GridCraft's [`LiveKind`].
pub struct Grid;

/// One local edit, queued by the engine's observer and handed back to the mapping at the next
/// flush.
#[derive(Clone, Debug)]
pub enum GridEdit {
    /// A top-level command that changed the workbook, as the engine reported it.
    Command(Box<EditReport>),
    /// The user asked for Undo: this editor's own last change is taken back ([`crate::undo`]).
    Undo,
    /// The user asked for Redo (see [`GridEdit::Undo`]).
    Redo,
    /// `formulas.calculateNow`: bumps the room's recalculation epoch, so every peer draws new
    /// random numbers.
    CalculateNow,
    /// Bring deleted lines back (the undo of a delete): an `Undelete` op.
    Undelete { sheet: Id, axis: crate::axis::AxisKind, ids: Vec<Id> },
}

/// Where another editor's selection is, as the host draws it. Positions are today's, resolved
/// through the layout.
#[derive(Clone, Debug, PartialEq)]
pub struct GridPos {
    /// The sheet's index.
    pub sheet: usize,
    pub active: CellRef,
    pub ranges: Vec<RangeRef>,
    /// The peer has its cell editor open on `active`.
    pub editing: bool,
}

/// Another editor's selection as the mapping keeps it between edits: by stable id, so it follows
/// its cells through row and column inserts and deletes, with the position it resolves to now.
#[derive(Clone, Debug, PartialEq)]
pub struct GridCaret {
    pub sheet: Id,
    /// The active cell's slot: (row id, column id).
    pub active: (Id, Id),
    /// Each range's corner slots.
    pub ranges: Vec<((Id, Id), (Id, Id))>,
    pub editing: bool,
    pub(crate) pos: GridPos,
}

impl GridCaret {
    /// The position this caret resolves to now.
    #[must_use]
    pub fn at(&self) -> &GridPos {
        &self.pos
    }
}

impl LiveKind for Grid {
    type Replica = GridDoc;
    type Edit = GridEdit;
    type Caret = GridCaret;
    type Pos = GridPos;

    fn pos(caret: &GridCaret) -> GridPos {
        caret.pos.clone()
    }
}
