use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rational {
    pub num: i64,
    pub den: i64,
}

impl Rational {
    pub const fn new(num: i64, den: i64) -> Self {
        Self { num, den }
    }
    pub fn as_f64(self) -> f64 {
        if self.den == 0 { 0.0 } else { self.num as f64 / self.den as f64 }
    }
    pub fn invert(self) -> Self {
        Self { num: self.den, den: self.num }
    }
    /// Rescale `value` from time base `from` to `to`, rounding to nearest.
    pub fn rescale(value: i64, from: Rational, to: Rational) -> i64 {
        let n = value as i128 * from.num as i128 * to.den as i128;
        let d = from.den as i128 * to.num as i128;
        if d == 0 {
            return 0;
        }
        let half = d.abs() / 2;
        (if n >= 0 { (n + half) / d } else { (n - half) / d }) as i64
    }
}

impl std::fmt::Display for Rational {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.num, self.den)
    }
}
