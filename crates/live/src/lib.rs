//! GridCraft live co-editing (layer 6).
//!
//! One yrs document per room holds only the changes against the pinned base workbook. This
//! crate holds the yrs schema ([`GridDoc`]), the row and column layout ([`Axis`], [`Layout`]),
//! and the op log ([`Op`]); [`GridLive`] is the session wrapper on top of `craft-live`: it maps the
//! engine's edit reports onto free writes (cells, formats, sizes, hidden flags), applies the other
//! editors' changes in one batch per update, and gates the commands ([`gate`]). Ordered edits
//! (rows and columns) are G9, per-user undo is G11.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]
#![forbid(unsafe_code)]

mod app;
pub mod axis;
pub mod cells;
pub mod doc;
pub mod gate;
pub mod ids;
pub mod kind;
pub mod layout;
mod local;
pub mod op;
mod ops;
pub mod ordered;
mod remote;
pub mod reprint;
pub mod schema;
pub mod session;
pub mod split;
pub mod styles;
pub mod volatile;
mod worker;

pub use app::SentCommand;
pub use axis::{Axis, AxisKind, Insertion};
pub use doc::{CellContent, Entry, GridChange, GridDoc, GridSeed, GridTxn, Meta};
pub use gate::NOT_YET_TEXT;
pub use ids::{Id, cell_key, parse_cell_key};
pub use kind::{Grid, GridCaret, GridEdit, GridPos};
pub use layout::{Hosted, Layout, SheetLayout};
pub use op::Op;
pub use ops::{LINES_MAX, TOO_MANY_TEXT};
pub use ordered::Tracked;
pub use session::{BaseFile, GridLive, LiveFail, Peer, Ran, schema, volatile_for, workbook_from_state};
pub use volatile::LiveVolatile;
pub use worker::GridWorkerLive;

/// Rows, for callers that name an axis without importing the formula crate.
#[must_use]
pub fn axis_kind_rows() -> AxisKind {
    AxisKind::Rows
}
