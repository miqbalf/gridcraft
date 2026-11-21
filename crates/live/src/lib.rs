//! GridCraft live co-editing (layer 6).
//!
//! One yrs document per room holds only the changes against the pinned base workbook. This
//! crate will hold the yrs schema, the row and column layout, the mapping to engine commands,
//! the gate for unshared commands, per-user undo and presence, on top of `craft-live`.
//! The crate is empty for now; the tasks of milestone M1 fill it.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]
#![forbid(unsafe_code)]
