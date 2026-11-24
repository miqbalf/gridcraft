//! The unmerged copy: when craft-live is about to drop local edits the room does not have (an
//! ordered run taken back, a replica that needs a resync, is damaged, was replaced by the room's
//! snapshot, or was refused), the replica's state with them is kept, and the host saves it beside
//! the file as `"{stem} (unmerged YYYY-MM-DD HHMM).xlsx"`, so the person loses nothing.

use chrono::NaiveDateTime;
use gridcraft_model::Workbook;

use crate::session::workbook_from_state;

/// Told when a copy starts waiting: the host offers it ([`crate::GridLive::take_unmerged`]).
pub const UNMERGED_TEXT: &str = "Some of your changes could not be merged with the other editors. A copy with them is ready to save.";

/// The name of the unmerged copy of `file_name` made at `at`:
/// `"Book (unmerged 2026-11-24 1404).xlsx"` for `"Book.xlsx"`.
#[must_use]
pub fn unmerged_name(file_name: &str, at: NaiveDateTime) -> String {
    let base = file_name.rsplit(['/', '\\']).next().unwrap_or(file_name);
    let stem = match base.rsplit_once('.') {
        Some((stem, _)) if !stem.is_empty() => stem,
        _ => base,
    };
    format!("{stem} (unmerged {}).xlsx", at.format("%Y-%m-%d %H%M"))
}

/// The replica's states with the local edits the room lacks, waiting for the host: one per
/// hand-over, oldest first, merged into one copy when taken.
#[derive(Default)]
pub(crate) struct Unmerged {
    pub states: Vec<Vec<u8>>,
    /// When it was handed over (stamped after the pump, on the wall clock).
    pub at: Option<NaiveDateTime>,
}

/// The `.xlsx` bytes of `base` with every state in `states` applied (yrs updates of one document,
/// so their merge holds each one's edits).
pub(crate) fn copy_bytes(base: &Workbook, states: &[Vec<u8>]) -> Result<Vec<u8>, String> {
    let merged;
    let state = match states {
        [one] => one.as_slice(),
        _ => {
            let parts: Vec<&[u8]> = states.iter().map(Vec::as_slice).collect();
            merged = craft_collab::yrs::merge_updates_v1(&parts).map_err(|e| e.to_string())?;
            merged.as_slice()
        }
    };
    let wb = workbook_from_state(base, state, None).map_err(|e| e.to_string())?;
    gridcraft_xlsx::write_xlsx(&wb).map_err(|e| e.to_string())
}

/// The wall clock on this target, when it has one.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn local_now() -> Option<NaiveDateTime> {
    Some(chrono::Local::now().naive_local())
}

/// The web has no clock here: the host gives one ([`crate::GridLive::set_wall_clock`]).
#[cfg(target_arch = "wasm32")]
pub(crate) fn local_now() -> Option<NaiveDateTime> {
    None
}
