//! Random numbers every peer agrees on (`RAND`, `RANDBETWEEN`, `RANDARRAY`).
//!
//! Each draw is a hash of the room's seed, its recalculation epoch, the sheet's id, the cell's
//! slot ids (the M1 content key, Ruling R22) and the draw's index inside the formula. It depends
//! on the cell's identity, not on the order the engine evaluates in, so every converged peer and
//! the worker show the same numbers. `NOW` and `TODAY` stay local (the source gives no clock).

use std::sync::{Arc, PoisonError, RwLock};

use gridcraft_calc::VolatileSource;
use gridcraft_core::CellRef;
use gridcraft_model::Workbook;

use crate::doc::GridDoc;
use crate::layout::Layout;

#[derive(Debug)]
struct State {
    seed: u64,
    epoch: u64,
    layout: Layout,
}

/// The [`VolatileSource`] of a live session. The mapping updates it as the epoch and the layout
/// move; the engine reads it while it calculates.
#[derive(Debug)]
pub struct LiveVolatile(RwLock<State>);

fn mix(h: u64, x: u64) -> u64 {
    let mut z = (h ^ x).wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

impl LiveVolatile {
    /// A source for `doc` over `base`.
    #[must_use]
    pub fn new(doc: &GridDoc, base: &Workbook) -> Arc<LiveVolatile> {
        let meta = doc.meta();
        Arc::new(LiveVolatile(RwLock::new(State { seed: meta.seed, epoch: meta.epoch.0, layout: Layout::from_doc(doc, base) })))
    }

    /// Takes the seed and epoch of `doc`.
    pub fn follow(&self, doc: &GridDoc) {
        let meta = doc.meta();
        let mut s = self.0.write().unwrap_or_else(PoisonError::into_inner);
        s.seed = meta.seed;
        s.epoch = meta.epoch.0;
    }

    /// Takes a new layout. Call it after every change of the row or column lines (an ordered op,
    /// local or remote) and before the recalculation that follows it: the engine asks by
    /// position and this maps the position to the line ids.
    pub fn update_layout(&self, layout: Layout) {
        self.0.write().unwrap_or_else(PoisonError::into_inner).layout = layout;
    }

    /// The seed and epoch count now in use.
    #[must_use]
    pub fn state(&self) -> (u64, u64) {
        let s = self.0.read().unwrap_or_else(PoisonError::into_inner);
        (s.seed, s.epoch)
    }
}

impl VolatileSource for LiveVolatile {
    fn random(&self, sheet: usize, cell: CellRef, draw: u32) -> f64 {
        let s = self.0.read().unwrap_or_else(PoisonError::into_inner);
        let sheet_id = s.layout.sheets.get(sheet).map_or(sheet as u64, |l| l.id.0);
        let (row, col) = s.layout.slot_at(sheet, cell).map_or((u64::from(cell.row), u64::from(cell.col)), |(r, c)| (r.0, c.0));
        debug_assert!(s.layout.sheets.get(sheet).is_some(), "random: sheet {sheet} is in the layout");
        debug_assert!(s.layout.slot_at(sheet, cell).is_some(), "random: the line ids of {cell:?} on sheet {sheet} resolve");
        let h = [s.epoch, sheet_id, row, col, u64::from(draw)].into_iter().fold(mix(s.seed, 0x5241_4e44), mix);
        // The top 53 bits over 2^53: integer shifts and one exact conversion, so the bits are
        // the same native and in wasm. No float math is involved (`fmath` has nothing to add).
        let x = (h >> 11) as f64 / (1u64 << 53) as f64;
        debug_assert!((0.0..1.0).contains(&x), "random: {x} is in [0, 1)");
        x
    }
}
