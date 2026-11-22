//! Transcendental maths through `libm`, so every target (native and wasm) computes the same
//! bits. `std`'s versions call the platform's maths library, which differs between targets, and
//! two peers editing one workbook must see the same numbers. Exactly rounded IEEE operations
//! (`sqrt`, `abs`, `floor`, ...) are identical everywhere and keep using `std`.

/// `sin` of `x`, computed by `libm`.
#[inline]
#[must_use]
pub fn sin(x: f64) -> f64 {
    libm::sin(x)
}

/// `cos` of `x`, computed by `libm`.
#[inline]
#[must_use]
pub fn cos(x: f64) -> f64 {
    libm::cos(x)
}

/// `tan` of `x`, computed by `libm`.
#[inline]
#[must_use]
pub fn tan(x: f64) -> f64 {
    libm::tan(x)
}

/// `asin` of `x`, computed by `libm`.
#[inline]
#[must_use]
pub fn asin(x: f64) -> f64 {
    libm::asin(x)
}

/// `acos` of `x`, computed by `libm`.
#[inline]
#[must_use]
pub fn acos(x: f64) -> f64 {
    libm::acos(x)
}

/// `atan` of `x`, computed by `libm`.
#[inline]
#[must_use]
pub fn atan(x: f64) -> f64 {
    libm::atan(x)
}

/// `sinh` of `x`, computed by `libm`.
#[inline]
#[must_use]
pub fn sinh(x: f64) -> f64 {
    libm::sinh(x)
}

/// `cosh` of `x`, computed by `libm`.
#[inline]
#[must_use]
pub fn cosh(x: f64) -> f64 {
    libm::cosh(x)
}

/// `tanh` of `x`, computed by `libm`.
#[inline]
#[must_use]
pub fn tanh(x: f64) -> f64 {
    libm::tanh(x)
}

/// `asinh` of `x`, computed by `libm`.
#[inline]
#[must_use]
pub fn asinh(x: f64) -> f64 {
    libm::asinh(x)
}

/// `acosh` of `x`, computed by `libm`.
#[inline]
#[must_use]
pub fn acosh(x: f64) -> f64 {
    libm::acosh(x)
}

/// `atanh` of `x`, computed by `libm`.
#[inline]
#[must_use]
pub fn atanh(x: f64) -> f64 {
    libm::atanh(x)
}

/// `exp` of `x`, computed by `libm`.
#[inline]
#[must_use]
pub fn exp(x: f64) -> f64 {
    libm::exp(x)
}

/// `exp_m1` of `x`, computed by `libm`.
#[inline]
#[must_use]
pub fn exp_m1(x: f64) -> f64 {
    libm::expm1(x)
}

/// `ln` of `x`, computed by `libm`.
#[inline]
#[must_use]
pub fn ln(x: f64) -> f64 {
    libm::log(x)
}

/// `ln_1p` of `x`, computed by `libm`.
#[inline]
#[must_use]
pub fn ln_1p(x: f64) -> f64 {
    libm::log1p(x)
}

/// `log10` of `x`, computed by `libm`.
#[inline]
#[must_use]
pub fn log10(x: f64) -> f64 {
    libm::log10(x)
}

/// `log2` of `x`, computed by `libm`.
#[inline]
#[must_use]
pub fn log2(x: f64) -> f64 {
    libm::log2(x)
}

/// `cbrt` of `x`, computed by `libm`.
#[inline]
#[must_use]
pub fn cbrt(x: f64) -> f64 {
    libm::cbrt(x)
}

/// `atan2(y, x)`, computed by `libm`.
#[inline]
#[must_use]
pub fn atan2(y: f64, x: f64) -> f64 {
    libm::atan2(y, x)
}

/// `pow(x, y)`, computed by `libm`.
#[inline]
#[must_use]
pub fn pow(x: f64, y: f64) -> f64 {
    libm::pow(x, y)
}

/// `hypot(x, y)`, computed by `libm`.
#[inline]
#[must_use]
pub fn hypot(x: f64, y: f64) -> f64 {
    libm::hypot(x, y)
}
