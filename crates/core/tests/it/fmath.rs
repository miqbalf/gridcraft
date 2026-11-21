//! `fmath` wraps `libm`. These tests only guard against a call mapped to the wrong function
//! (`ln` used for `log10`); they do not test `libm` itself.

use gridcraft_core::fmath;

/// Seeded generator in the house style of the engine fuzz tests (SplitMix64).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    /// Uniform in `[lo, hi)`.
    fn range(&mut self, lo: f64, hi: f64) -> f64 {
        lo + (self.next() >> 11) as f64 / (1u64 << 53) as f64 * (hi - lo)
    }
}

/// Distance in representable doubles between `a` and `b`.
fn ulps(a: f64, b: f64) -> u64 {
    if a == b {
        return 0;
    }
    if a.is_nan() || b.is_nan() {
        return if a.is_nan() && b.is_nan() { 0 } else { u64::MAX };
    }
    let key = |x: f64| {
        let bits = x.to_bits() as i64;
        if bits < 0 { i64::MIN.wrapping_sub(bits) } else { bits }
    };
    key(a).abs_diff(key(b))
}

const SEED: u64 = 0x6772_6964_6372_6166;
const SAMPLES: usize = 400;

fn sweep(name: &str, lo: f64, hi: f64, ours: fn(f64) -> f64, std: fn(f64) -> f64) {
    sweep_within(name, 2, lo, hi, ours, std);
}

fn sweep_within(name: &str, max_ulps: u64, lo: f64, hi: f64, ours: fn(f64) -> f64, std: fn(f64) -> f64) {
    let mut rng = Rng(SEED);
    for _ in 0..SAMPLES {
        let x = rng.range(lo, hi);
        let d = ulps(ours(x), std(x));
        assert!(d <= max_ulps, "{name}({x}) differs from std by {d} ulps (seed {SEED:#x})");
    }
}

fn sweep2(name: &str, (lo_a, hi_a): (f64, f64), (lo_b, hi_b): (f64, f64), ours: fn(f64, f64) -> f64, std: fn(f64, f64) -> f64) {
    let mut rng = Rng(SEED);
    for _ in 0..SAMPLES {
        let (a, b) = (rng.range(lo_a, hi_a), rng.range(lo_b, hi_b));
        let d = ulps(ours(a, b), std(a, b));
        assert!(d <= 2, "{name}({a}, {b}) differs from std by {d} ulps (seed {SEED:#x})");
    }
}

#[test]
fn fmath_agrees_with_std_within_two_ulps() {
    sweep("sin", -100.0, 100.0, fmath::sin, f64::sin);
    sweep("cos", -100.0, 100.0, fmath::cos, f64::cos);
    sweep("tan", -1.5, 1.5, fmath::tan, f64::tan);
    sweep("asin", -1.0, 1.0, fmath::asin, f64::asin);
    sweep("acos", -1.0, 1.0, fmath::acos, f64::acos);
    sweep("atan", -1e6, 1e6, fmath::atan, f64::atan);
    sweep("sinh", -20.0, 20.0, fmath::sinh, f64::sinh);
    sweep("cosh", -20.0, 20.0, fmath::cosh, f64::cosh);
    sweep("tanh", -20.0, 20.0, fmath::tanh, f64::tanh);
    sweep("asinh", -1e6, 1e6, fmath::asinh, f64::asinh);
    sweep("acosh", 1.0, 1e6, fmath::acosh, f64::acosh);
    // libm's atanh is a few ulps less exact than the platform's, so it gets a wider bound.
    sweep_within("atanh", 4, -0.9, 0.9, fmath::atanh, f64::atanh);
    sweep("exp", -50.0, 50.0, fmath::exp, f64::exp);
    sweep("exp_m1", -5.0, 5.0, fmath::exp_m1, f64::exp_m1);
    sweep("ln", 1e-9, 1e9, fmath::ln, f64::ln);
    sweep("ln_1p", -0.9, 1e6, fmath::ln_1p, f64::ln_1p);
    sweep("log10", 1e-9, 1e9, fmath::log10, f64::log10);
    sweep("log2", 1e-9, 1e9, fmath::log2, f64::log2);
    sweep("cbrt", -1e9, 1e9, fmath::cbrt, f64::cbrt);
    sweep2("atan2", (-100.0, 100.0), (-100.0, 100.0), fmath::atan2, f64::atan2);
    sweep2("pow", (0.01, 100.0), (-10.0, 10.0), fmath::pow, f64::powf);
    sweep2("hypot", (-1e6, 1e6), (-1e6, 1e6), fmath::hypot, f64::hypot);
}

#[test]
fn fmath_functions_are_not_mixed_up() {
    // Relations between functions that a wrong mapping would break.
    let x = 7.25_f64;
    assert!((fmath::ln(x) / fmath::ln(10.0) - fmath::log10(x)).abs() < 1e-12);
    assert!((fmath::ln(x) / fmath::ln(2.0) - fmath::log2(x)).abs() < 1e-12);
    assert!((fmath::exp(fmath::ln(x)) - x).abs() < 1e-12);
    assert!((fmath::pow(x, 3.0) - fmath::cbrt(fmath::pow(x, 9.0))).abs() < 1e-9);
    assert!((fmath::sin(x).powi(2) + fmath::cos(x).powi(2) - 1.0).abs() < 1e-12);
    assert!((fmath::hypot(3.0, 4.0) - 5.0).abs() < 1e-12);
    assert!((fmath::exp_m1(1e-10) - (fmath::exp(1e-10) - 1.0)).abs() < 1e-15);
}
