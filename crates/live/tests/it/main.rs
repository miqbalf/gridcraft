//! The one integration test binary of `gridcraft-live`; modules sit beside this file.

mod axis;
mod doc;
#[cfg(any(feature = "test-util", feature = "native-hub"))]
mod fingerprint;
#[cfg(feature = "test-util")]
mod free;
#[cfg(feature = "test-util")]
mod gate;
#[cfg(feature = "test-util")]
mod junk;
#[cfg(feature = "test-util")]
mod ordered;
#[cfg(feature = "test-util")]
mod presence;
#[cfg(feature = "native-hub")]
mod relay;
#[cfg(feature = "test-util")]
mod support;
#[cfg(feature = "test-util")]
mod trace;
#[cfg(feature = "test-util")]
mod undo;
#[cfg(feature = "test-util")]
mod unmerged;
#[cfg(feature = "test-util")]
mod volatile;

/// SplitMix64, the house generator for seeded random tests (as in the engine's
/// `command_sequence_fuzz`): tiny and deterministic. Each test prints its seed on failure.
pub struct Rng(pub u64);

impl Rng {
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    pub fn below(&mut self, n: usize) -> usize {
        if n == 0 { 0 } else { (self.next_u64() % n as u64) as usize }
    }
    pub fn chance(&mut self, percent: u64) -> bool {
        self.next_u64() % 100 < percent
    }
    /// The indices `0..n` in a random order, some of them twice.
    pub fn order_with_duplicates(&mut self, n: usize) -> Vec<usize> {
        let mut out: Vec<usize> = (0..n).collect();
        for i in (1..out.len()).rev() {
            let j = self.below(i + 1);
            out.swap(i, j);
        }
        let dups: Vec<usize> = out.iter().copied().filter(|_| self.chance(20)).collect();
        for d in dups {
            let at = self.below(out.len() + 1);
            out.insert(at, d);
        }
        out
    }
}

/// How many seeds each random test runs: `GRIDCRAFT_LIVE_SEEDS`, default 24.
pub fn seeds() -> Vec<u64> {
    seeds_or(24)
}

/// The seeds of a random test: `GRIDCRAFT_LIVE_SEED` runs one, else `GRIDCRAFT_LIVE_SEEDS` of
/// them, else `default`.
pub fn seeds_or(default: u64) -> Vec<u64> {
    match std::env::var("GRIDCRAFT_LIVE_SEED").ok().and_then(|v| v.trim().parse().ok()) {
        Some(one) => vec![one],
        None => (0..std::env::var("GRIDCRAFT_LIVE_SEEDS").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(default)).collect(),
    }
}
