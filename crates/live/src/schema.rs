//! The `gridcraft/1` yrs schema, M1 roots.
//!
//! ```text
//! "meta"    YMap    "base": sha256 hex of the base bytes; "seed": RAND seed (base 36 text);
//!                   "epoch": [n, by]
//! "sheets"  YArray  sheet ids in tab order
//! "sheet"   YMap    sheet id (base 36) -> YMap:
//!     "rows", "cols"       YArray  explicit prefix of line ids
//!     "rows_x", "cols_x"   YMap    deleted line id -> true
//!     "cells"              YMap    "r.c" -> cell content; null = cleared
//!     "fmt"                YMap    "r.c" -> style key; null = cleared
//!     "rinfo", "cinfo"     YMap    "<id>/<p>" -> line property
//!     props                "defw", "defh", …
//! "styles"  YMap    style key -> canonical style JSON
//! "ops"     YArray  the op log
//! ```
//!
//! An absent key means "the base file's"; `null` means cleared. `lanes`, `lane_of` and `over`
//! are not created in M1: M2 adds them, and the schema build id keeps M1 and M2 peers apart.
//! The epoch is a two-element array rather than a map, because yrs encodes a nested map in hash
//! order and the seed must give equal bytes on every replica.

use crate::axis::AxisKind;

pub const META: &str = "meta";
pub const SHEETS: &str = "sheets";
pub const SHEET: &str = "sheet";
pub const STYLES: &str = "styles";
pub const OPS: &str = "ops";

pub const META_BASE: &str = "base";
pub const META_SEED: &str = "seed";
pub const META_EPOCH: &str = "epoch";

pub const ROWS: &str = "rows";
pub const ROWS_X: &str = "rows_x";
pub const COLS: &str = "cols";
pub const COLS_X: &str = "cols_x";
pub const CELLS: &str = "cells";
pub const FMT: &str = "fmt";
pub const RINFO: &str = "rinfo";
pub const CINFO: &str = "cinfo";

/// The keys of a sheet's map that hold nested types. A sheet property never takes one of them.
pub const SHEET_PARTS: [&str; 8] = [CELLS, CINFO, COLS, COLS_X, FMT, RINFO, ROWS, ROWS_X];

/// The array of line ids and the map of deleted ones for `axis`.
#[must_use]
pub fn axis_parts(axis: AxisKind) -> (&'static str, &'static str) {
    match axis {
        AxisKind::Rows => (ROWS, ROWS_X),
        AxisKind::Cols => (COLS, COLS_X),
    }
}

/// The line-property map for `axis`.
#[must_use]
pub fn info_part(axis: AxisKind) -> &'static str {
    match axis {
        AxisKind::Rows => RINFO,
        AxisKind::Cols => CINFO,
    }
}

/// Lowercase hex.
#[must_use]
pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}
