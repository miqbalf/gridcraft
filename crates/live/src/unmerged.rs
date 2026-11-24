//! The unmerged copy: when craft-live is about to drop local edits the room does not have (an
//! ordered run taken back, a replica that needs a resync, is damaged, was replaced by the room's
//! snapshot, or was refused), the replica's state with them is kept, and the host saves it beside
//! the file as `"{stem} (unmerged YYYY-MM-DD HHMMSS).xlsx"`, so the person loses nothing.
//!
//! A kept state that cannot be built into a workbook does not sink the others: the states are then
//! built one by one, and one that still fails is handed over as it is, a recovery file
//! (`.yrs`, the replica's own bytes) that support can restore. Nothing kept is ever dropped.

use std::collections::HashSet;

use chrono::NaiveDateTime;
use gridcraft_model::Workbook;

use crate::session::workbook_from_state;

/// Told when a copy is ready (not for a run taken back: its own notice offers the copy).
pub const UNMERGED_TEXT: &str = "Some of your changes could not be merged with the other editors. A copy with them is ready to save.";

/// Told when some kept edits could not be built into a workbook: their recovery file is offered.
pub const RECOVERY_TEXT: &str =
    "Some of your changes could not be turned into a workbook. A recovery file with them is ready to save; support can restore it.";

/// The notices that say a run was taken back: the copy of it is offered on that notice, and
/// [`UNMERGED_TEXT`] is not told as well.
pub const TAKE_BACK_TEXTS: &[&str] = &[
    crate::ordered::LOCK_LOST_TEXT,
    crate::ordered::LOCK_LOST_LINK_TEXT,
    crate::ordered::LOCK_LOST_UNRUN_TEXT,
    crate::ordered::LOCK_LOST_UNRUN_LINK_TEXT,
    crate::ordered::TAKEN_BACK_TEXT,
];

/// One copy of unmerged edits, for the host to offer or save.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnmergedCopy {
    /// The file name to save it under (unique in this session).
    pub name: String,
    /// The `.xlsx` workbook, or for a recovery file the replica's state as it was kept.
    pub bytes: Vec<u8>,
    /// It holds a run that was taken back: offer it, never save it without the person asking.
    pub taken_back: bool,
    /// A recovery file (`.yrs`), not a workbook.
    pub recovery: bool,
}

/// The name of the unmerged copy of `file_name` made at `at`:
/// `"Book (unmerged 2026-11-24 140405).xlsx"` for `"Book.xlsx"`.
#[must_use]
pub fn unmerged_name(file_name: &str, at: NaiveDateTime) -> String {
    format!("{} (unmerged {}).xlsx", stem(file_name), at.format("%Y-%m-%d %H%M%S"))
}

/// The file name without its folder and extension.
fn stem(file_name: &str) -> &str {
    let base = file_name.rsplit(['/', '\\']).next().unwrap_or(file_name);
    match base.rsplit_once('.') {
        Some((stem, _)) if !stem.is_empty() => stem,
        _ => base,
    }
}

/// The replica's states with the local edits the room lacks, waiting for the host: one per
/// hand-over, oldest first, merged into one copy when taken.
#[derive(Default)]
pub(crate) struct Unmerged {
    pub states: Vec<Vec<u8>>,
    /// When it was handed over (stamped after the pump, on the wall clock).
    pub at: Option<NaiveDateTime>,
    /// A run was taken back while it waited.
    pub taken_back: bool,
}

/// The names given in this session, so no two copies share one.
#[derive(Default)]
pub(crate) struct Names {
    used: HashSet<String>,
    /// Copies named without a clock.
    counted: u32,
}

impl Names {
    /// A name for a copy of `file_name` made at `at` (a counted name with no clock), with
    /// `ext`, never one given before.
    pub(crate) fn next(&mut self, file_name: &str, at: Option<NaiveDateTime>, ext: &str) -> String {
        let stem = stem(file_name);
        let base = match at {
            Some(at) => format!("{stem} (unmerged {})", at.format("%Y-%m-%d %H%M%S")),
            None => {
                self.counted += 1;
                format!("{stem} (unmerged copy {})", self.counted)
            }
        };
        let mut name = format!("{base}.{ext}");
        let mut n = 1;
        while !self.used.insert(name.clone()) {
            n += 1;
            name = format!("{base} {n}.{ext}");
        }
        name
    }
}

/// What the kept states build into: the `.xlsx` workbooks, and the states no workbook could be
/// built from (kept as they are).
pub(crate) struct Built {
    pub workbooks: Vec<Vec<u8>>,
    pub unbuilt: Vec<Vec<u8>>,
}

/// Builds `states` on `base`: all merged into one workbook when they can be; else each state that
/// builds on its own (merged together when that works), and the rest as they are.
pub(crate) fn build(base: &Workbook, states: &[Vec<u8>]) -> Built {
    if let Ok(bytes) = merged_bytes(base, states) {
        return Built { workbooks: vec![bytes], unbuilt: Vec::new() };
    }
    let mut good = Vec::new();
    let mut unbuilt = Vec::new();
    for s in states {
        match copy_bytes(base, s) {
            Ok(bytes) => good.push((s.clone(), bytes)),
            Err(e) => {
                log::warn!("gridcraft-live: a kept state does not build into a workbook ({e}); it is offered as a recovery file");
                unbuilt.push(s.clone());
            }
        }
    }
    let workbooks = match good.as_slice() {
        [] => Vec::new(),
        [(_, one)] => vec![one.clone()],
        _ => {
            let states: Vec<Vec<u8>> = good.iter().map(|(s, _)| s.clone()).collect();
            merged_bytes(base, &states).map_or_else(|_| good.into_iter().map(|(_, b)| b).collect(), |b| vec![b])
        }
    };
    Built { workbooks, unbuilt }
}

/// The `.xlsx` bytes of `base` with every state in `states` applied (yrs updates of one document,
/// so their merge holds each one's edits).
fn merged_bytes(base: &Workbook, states: &[Vec<u8>]) -> Result<Vec<u8>, String> {
    match states {
        [one] => copy_bytes(base, one),
        _ => {
            let parts: Vec<&[u8]> = states.iter().map(Vec::as_slice).collect();
            let merged = craft_collab::yrs::merge_updates_v1(&parts).map_err(|e| e.to_string())?;
            copy_bytes(base, &merged)
        }
    }
}

fn copy_bytes(base: &Workbook, state: &[u8]) -> Result<Vec<u8>, String> {
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
