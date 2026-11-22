//! A local copy of G2's frozen `gridcraft_formula::ids::{Resolver, Lookup}`.
//!
//! G2 adds the trait to `gridcraft-formula` in parallel with this crate's layout. Until it lands,
//! the layout implements this copy, which has the same signature. At the merge, this file is
//! deleted and the uses point at `gridcraft_formula::ids`.

use gridcraft_formula::adjust::Axis;

/// Where an id is now on its axis.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lookup {
    /// Shown at this 0-based position.
    At(u32),
    /// Deleted. `before` and `after` are the positions of the nearest shown lines on either side,
    /// where there are any.
    Deleted { before: Option<u32>, after: Option<u32> },
    /// Not an id this axis knows (or pushed past the sheet's last line).
    Unknown,
}

/// What the id form needs to know about the current layout.
pub trait Resolver {
    fn sheet_id(&self, name: &str) -> Option<u64>;
    fn sheet_name(&self, id: u64) -> Option<String>;
    /// `sheet` None = the formula's own sheet.
    fn line_id(&self, sheet: Option<u64>, axis: Axis, pos: u32) -> Option<u64>;
    fn line_pos(&self, sheet: Option<u64>, axis: Axis, id: u64) -> Lookup;
}
