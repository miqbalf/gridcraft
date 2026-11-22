//! GridCraft live co-editing (layer 6).
//!
//! One yrs document per room holds only the changes against the pinned base workbook. This
//! crate holds the yrs schema ([`GridDoc`]), the row and column layout ([`Axis`], [`Layout`]),
//! and the op log ([`Op`]); the mapping to engine commands, the gate for unshared commands,
//! per-user undo and presence follow in later M1 tasks, on top of `craft-live`.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]
#![forbid(unsafe_code)]

pub mod axis;
pub mod doc;
pub mod formula_ids;
pub mod ids;
pub mod layout;
pub mod op;
pub mod schema;

pub use axis::{Axis, AxisKind, Insertion};
pub use doc::{CellContent, Entry, GridChange, GridDoc, GridSeed, GridTxn, HELD_MAX_BYTES, HELD_MAX_ENTRIES, Meta};
pub use ids::{Id, cell_key, parse_cell_key};
pub use layout::{Hosted, Layout, SheetLayout};
pub use op::Op;
