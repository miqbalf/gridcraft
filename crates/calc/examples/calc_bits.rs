//! Prints the bits of every transcendental worksheet function over 200 inputs, one cell per line
//! as `sheet!cell<TAB>value bits as hex`. Run natively and as wasm (`wasm32-wasip1`): the two
//! outputs must be identical, so two peers on different targets always see the same numbers.
//!
//! `cargo run --release -p gridcraft-calc --example calc_bits > native.txt`

use gridcraft_calc::Calc;
use gridcraft_core::{CellRef, Value, col_to_letters};
use gridcraft_model::{Cell, Formula, Workbook};

const INPUTS: usize = 200;

/// SplitMix64: a seeded generator, so every target gets the same inputs.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn range(&mut self, lo: f64, hi: f64) -> f64 {
        lo + (self.next() >> 11) as f64 / (1u64 << 53) as f64 * (hi - lo)
    }
}

/// Edge values first (zero, unit, tiny, huge, the overflow and underflow edges of `exp`), then
/// seeded values spread over several magnitudes.
fn first_inputs() -> Vec<f64> {
    let mut v = vec![
        0.0,
        1.0,
        -1.0,
        0.5,
        -0.5,
        2.0,
        -2.0,
        std::f64::consts::PI,
        -std::f64::consts::PI,
        std::f64::consts::FRAC_PI_2,
        1e-300,
        -1e-300,
        1e-8,
        1e8,
        709.0,
        710.0,
        -745.0,
        100.0,
        -100.0,
        1e300,
        37.0,
        0.999999,
        -0.999999,
        1.000001,
    ];
    let mut rng = Rng(0x6772_6964);
    while v.len() < INPUTS {
        let scale = [0.01, 1.0, 10.0, 50.0][(rng.next() % 4) as usize];
        v.push(rng.range(-1.0, 1.0) * scale);
    }
    v
}

fn second_inputs() -> Vec<f64> {
    let mut v = vec![0.0, 1.0, 2.0, 0.5, 10.0, 1e-8, 100.0];
    let mut rng = Rng(0x6372_6166);
    while v.len() < INPUTS {
        v.push(rng.range(0.01, 8.0));
    }
    v
}

/// Worksheet formulas over `A{row}` (first input) and `B{row}` (second input).
const FORMULAS: &[&str] = &[
    "SIN(A{r})",
    "COS(A{r})",
    "TAN(A{r})",
    "ASIN(A{r})",
    "ACOS(A{r})",
    "ATAN(A{r})",
    "ATAN2(A{r},B{r})",
    "SINH(A{r})",
    "COSH(A{r})",
    "TANH(A{r})",
    "ASINH(A{r})",
    "ACOSH(A{r})",
    "ATANH(A{r})",
    "EXP(A{r})",
    "LN(A{r})",
    "LOG10(A{r})",
    "LOG(A{r},B{r})",
    "POWER(A{r},B{r})",
    "A{r}^B{r}",
    "COT(A{r})",
    "SEC(A{r})",
    "CSC(A{r})",
    "COTH(A{r})",
    "SECH(A{r})",
    "CSCH(A{r})",
    "ACOT(A{r})",
    "ACOTH(A{r})",
    "FISHER(A{r})",
    "FISHERINV(A{r})",
    "GAMMALN(A{r})",
    "GAMMA(A{r})",
    "ERF(A{r})",
    "NORM.S.DIST(A{r},TRUE)",
    "NORM.S.DIST(A{r},FALSE)",
    "NORM.S.INV(A{r})",
    "LOGNORM.DIST(B{r},A{r},B{r},TRUE)",
    "EXPON.DIST(B{r},B{r},FALSE)",
    "WEIBULL.DIST(B{r},B{r},B{r},TRUE)",
    "BESSELJ(B{r},2)",
    "BESSELI(B{r},1)",
    "BESSELY(B{r},0)",
    "FV(0.05,B{r},-100,0)",
    "PMT(0.05,B{r},1000)",
    "NPER(0.05,-100,1000)",
    "EFFECT(0.05,B{r})",
    "NOMINAL(0.05,B{r})",
    "T.DIST(A{r},B{r}+1,TRUE)",
    "T.DIST(A{r},B{r}+1,FALSE)",
    "T.INV(0.3,B{r}+1)",
    "GAMMA.DIST(B{r},B{r},B{r},TRUE)",
    "GAMMA.DIST(B{r},B{r},B{r},FALSE)",
    "GAMMA.INV(0.4,B{r},B{r})",
    "BETA.DIST(0.3,B{r},B{r}+1,TRUE)",
    "BETA.DIST(0.3,B{r},B{r}+1,FALSE)",
    "BETA.INV(0.4,B{r},B{r}+1)",
    "CHISQ.DIST(B{r},B{r}+1,TRUE)",
    "CHISQ.DIST(B{r},B{r}+1,FALSE)",
    "CHISQ.INV(0.4,B{r}+1)",
    "F.DIST(B{r},B{r}+1,B{r}+2,TRUE)",
    "F.DIST(B{r},B{r}+1,B{r}+2,FALSE)",
    "F.INV(0.4,B{r}+1,B{r}+2)",
    "NORM.DIST(A{r},B{r},B{r}+1,TRUE)",
    "NORM.DIST(A{r},B{r},B{r}+1,FALSE)",
    "NORM.INV(0.3,B{r},B{r}+1)",
    "LOGNORM.INV(0.3,A{r},B{r})",
    "LOGNORM.DIST(B{r},A{r},B{r},FALSE)",
    "WEIBULL.DIST(B{r},B{r},B{r},FALSE)",
    "EXPON.DIST(B{r},B{r},TRUE)",
    "BINOM.DIST(3,10,MIN(1,B{r}/8),TRUE)",
    "BINOM.DIST(3,10,MIN(1,B{r}/8),FALSE)",
    "POISSON.DIST(3,B{r},TRUE)",
    "POISSON.DIST(3,B{r},FALSE)",
    "NEGBINOM.DIST(3,4,MIN(1,B{r}/8),FALSE)",
    "HYPGEOM.DIST(2,5,6,20,FALSE)",
    "CHISQ.TEST(A1:A5,B1:B5)",
    "PRICE(DATE(2024,1,15),DATE(2030,6,30),0.05,0.04+B{r}/100,100,2,0)",
    "YIELD(DATE(2024,1,15),DATE(2030,6,30),0.05,95+B{r},100,2,0)",
    "DURATION(DATE(2024,1,15),DATE(2030,6,30),0.05,0.04+B{r}/100,2,0)",
    "MDURATION(DATE(2024,1,15),DATE(2030,6,30),0.05,0.04+B{r}/100,2,0)",
    "DB(10000,1000,5+B{r},2,6)",
    "DDB(10000,1000,5+B{r},2,2)",
    "IRR($CW$1:$CW$5)",
    "XNPV(0.05,$CW$1:$CW$5,$CV$1:$CV$5)",
    "XIRR($CW$1:$CW$5,$CV$1:$CV$5)",
    "RATE(10,-100,800+B{r})",
    "PV(0.05,B{r},-100,0)",
    "IPMT(0.05,2,B{r}+3,1000)",
    "PPMT(0.05,2,B{r}+3,1000)",
    "MIRR($CW$1:$CW$5,0.05,0.06)",
    "NPV(0.05,$CW$1:$CW$5)",
    "LOGEST(B1:B20,A1:A20)",
    "GROWTH(B1:B20,A1:A20,B{r})",
    "TREND(B1:B20,A1:A20,B{r})",
    "SLOPE(B1:B20,A1:A20)",
    "STDEV.S(A1:A20)",
    "SKEW(B1:B20)",
    "KURT(B1:B20)",
    "GEOMEAN(B1:B20)",
    "COMBIN(B{r}*20+10,5)",
    "ROUND(A{r}*1000,2)",
    "POWER(2,A{r})",
    "LOG(B{r}+1)",
    "GAMMALN(B{r}+0.5)",
    "ERFC(A{r})",
    "CONFIDENCE.T(0.05,B{r}+1,20)",
    "PROB(A1:A5,B1:B5,1)",
    "BESSELK(B{r}+0.5,1)",
    "BESSELJ(B{r}+1,5)",
    "IMPRODUCT(COMPLEX(A{r},B{r}),COMPLEX(B{r},1))",
    "IMSQRT(COMPLEX(A{r},B{r}))",
    "IMCOS(COMPLEX(A{r},B{r}))",
    "IMSINH(COMPLEX(A{r},B{r}))",
    "IMLOG10(COMPLEX(A{r},B{r}))",
    "IMLOG2(COMPLEX(A{r},B{r}))",
    "IMARGUMENT(COMPLEX(A{r},B{r}))",
    "IMTAN(COMPLEX(A{r},B{r}))",
    "IMCOSH(COMPLEX(A{r},B{r}))",
    "RRI(B{r},100,A{r})",
    "GEOMEAN(B{r},A{r},2)",
    "IMABS(COMPLEX(A{r},B{r}))",
    "IMEXP(COMPLEX(A{r},B{r}))",
    "IMLN(COMPLEX(A{r},B{r}))",
    "IMSIN(COMPLEX(A{r},B{r}))",
    "IMPOWER(COMPLEX(A{r},B{r}),B{r})",
    "DEGREES(A{r})",
    "RADIANS(A{r})",
    "SQRT(B{r})",
];

fn cell(col: u32, row: u32) -> CellRef {
    CellRef::new(row, col)
}

fn main() {
    let mut wb = Workbook::new();
    let (firsts, seconds) = (first_inputs(), second_inputs());
    if let Some(sheet) = wb.sheet_mut(0) {
        for (i, (a, b)) in firsts.iter().zip(&seconds).enumerate() {
            let row = i as u32;
            sheet.cells.set(cell(0, row), Cell::value(Value::Number(*a)));
            sheet.cells.set(cell(1, row), Cell::value(Value::Number(*b)));
            for (f, text) in FORMULAS.iter().enumerate() {
                let a1 = text.replace("{r}", &(row + 1).to_string());
                sheet.cells.set(cell(2 + f as u32, row), Cell::formula(Formula::new(&format!("={a1}"))));
            }
        }
    }
    // A fixed cash-flow series with dates, for IRR, XIRR, XNPV, MIRR and NPV.
    if let Some(sheet) = wb.sheet_mut(0) {
        for (i, (flow, serial)) in [(-1000.0, 45292.0), (300.0, 45500.0), (400.0, 45800.0), (500.0, 46100.0), (200.0, 46400.0)].iter().enumerate() {
            sheet.cells.set(cell(99, i as u32), Cell::value(Value::Number(*serial)));
            sheet.cells.set(cell(100, i as u32), Cell::value(Value::Number(*flow)));
        }
    }
    let mut calc = Calc::new();
    calc.rebuild(&wb);
    calc.recalc_all(&mut wb);

    let mut out = String::new();
    if let Some(sheet) = wb.sheet(0) {
        for f in 0..FORMULAS.len() as u32 {
            for row in 0..INPUTS as u32 {
                let at = cell(2 + f, row);
                let shown = match sheet.value(at) {
                    Value::Number(n) => format!("{:016x}", n.to_bits()),
                    other => format!("{other:?}"),
                };
                out.push_str(&format!("{}!{}{}\t{shown}\n", sheet.name, col_to_letters(at.col), row + 1));
            }
        }
    }
    print!("{out}");
}
