//! Fixed-point numbers for prices, quantities and money.
//!
//! Every value is an `i64` count of 10^-8 units, so `1.5` is stored as
//! `150_000_000`. Floating point can't represent most decimal fractions
//! exactly (0.1 + 0.2 != 0.3), and over millions of fills those errors turn
//! into accounting that doesn't balance. Integers don't drift: two runs over
//! the same data produce bit-identical results, and cash + positions always
//! reconciles to the cent.
//!
//! Eight decimal places covers crypto (Bitcoin's smallest unit is 10^-8) and
//! leaves headroom of about ±92 billion per value. Multiplying a price by a
//! quantity goes through `i128` so the intermediate can't overflow, and the
//! result is rounded half-to-even so rounding errors don't bias in one
//! direction. Overflow always panics rather than wrapping silently.

use std::fmt;
use std::ops::{Add, AddAssign, Mul, Neg, Sub, SubAssign};
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Number of decimal places every fixed-point value carries.
pub const DECIMALS: usize = 8;

/// Raw units per whole unit: 10^[`DECIMALS`].
pub const SCALE: i64 = 100_000_000;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ParseFixedError {
    #[error("not a decimal number: {0:?}")]
    Invalid(String),
    #[error("{0:?} has more than {DECIMALS} decimal places")]
    TooPrecise(String),
    #[error("{0:?} is out of range")]
    Overflow(String),
}

fn parse_fixed(input: &str) -> Result<i64, ParseFixedError> {
    let invalid = || ParseFixedError::Invalid(input.to_owned());
    let s = input.trim();
    let (negative, body) = match s.as_bytes().first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    let (int_part, frac_part) = body.split_once('.').unwrap_or((body, ""));
    let all_digits = |p: &str| p.bytes().all(|b| b.is_ascii_digit());
    if (int_part.is_empty() && frac_part.is_empty())
        || !all_digits(int_part)
        || !all_digits(frac_part)
    {
        return Err(invalid());
    }

    // Exchanges sometimes pad with trailing zeros beyond our precision
    // ("0.0100000000"). That's fine; real extra precision is not.
    let frac_part = if frac_part.len() > DECIMALS {
        let (keep, extra) = frac_part.split_at(DECIMALS);
        if extra.bytes().any(|b| b != b'0') {
            return Err(ParseFixedError::TooPrecise(input.to_owned()));
        }
        keep
    } else {
        frac_part
    };

    let overflow = || ParseFixedError::Overflow(input.to_owned());
    let mut int_value: i128 = 0;
    for b in int_part.bytes() {
        int_value = int_value * 10 + i128::from(b - b'0');
        if int_value > i128::from(i64::MAX) {
            return Err(overflow());
        }
    }
    let mut frac_value: i128 = 0;
    for b in frac_part.bytes() {
        frac_value = frac_value * 10 + i128::from(b - b'0');
    }
    frac_value *= 10_i128.pow((DECIMALS - frac_part.len()) as u32);

    let magnitude = int_value * i128::from(SCALE) + frac_value;
    let value = if negative { -magnitude } else { magnitude };
    i64::try_from(value).map_err(|_| overflow())
}

fn format_fixed(raw: i64, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    let magnitude = raw.unsigned_abs();
    let scale = SCALE as u64;
    let (int_part, frac_part) = (magnitude / scale, magnitude % scale);
    if raw < 0 {
        f.write_str("-")?;
    }
    write!(f, "{int_part}")?;
    if frac_part != 0 {
        let digits = format!("{frac_part:0width$}", width = DECIMALS);
        write!(f, ".{}", digits.trim_end_matches('0'))?;
    }
    Ok(())
}

/// `n / d` rounded half-to-even. `d` must be positive.
fn div_round_half_even(n: i128, d: i128) -> i128 {
    debug_assert!(d > 0);
    let q = n.div_euclid(d);
    let twice_remainder = 2 * n.rem_euclid(d);
    if twice_remainder > d || (twice_remainder == d && q % 2 != 0) {
        q + 1
    } else {
        q
    }
}

fn to_i64(value: i128, what: &str) -> i64 {
    i64::try_from(value).unwrap_or_else(|_| panic!("fixed-point overflow in {what}"))
}

macro_rules! fixed_type {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Copy, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(i64);

        impl $name {
            pub const ZERO: Self = Self(0);

            /// Wraps a raw count of 10^-8 units.
            pub const fn from_raw(raw: i64) -> Self {
                Self(raw)
            }

            /// The raw count of 10^-8 units.
            pub const fn raw(self) -> i64 {
                self.0
            }

            /// A whole number of units, or `None` if it's out of range.
            pub const fn from_int(units: i64) -> Option<Self> {
                match units.checked_mul(SCALE) {
                    Some(raw) => Some(Self(raw)),
                    None => None,
                }
            }

            pub const fn is_zero(self) -> bool {
                self.0 == 0
            }

            pub const fn is_positive(self) -> bool {
                self.0 > 0
            }

            pub const fn is_negative(self) -> bool {
                self.0 < 0
            }

            pub fn abs(self) -> Self {
                Self(self.0.checked_abs().expect(concat!(stringify!($name), " overflow in abs")))
            }

            pub fn checked_add(self, rhs: Self) -> Option<Self> {
                self.0.checked_add(rhs.0).map(Self)
            }

            pub fn checked_sub(self, rhs: Self) -> Option<Self> {
                self.0.checked_sub(rhs.0).map(Self)
            }

            /// `self * num / den`, rounded half-to-even. Used for fee rates
            /// and other ratios without going through floating point.
            pub fn mul_ratio(self, num: i64, den: i64) -> Self {
                assert!(den != 0, "mul_ratio by zero denominator");
                let (num, den) = if den < 0 { (-i128::from(num), -i128::from(den)) } else { (i128::from(num), i128::from(den)) };
                let value = div_round_half_even(i128::from(self.0) * num, den);
                Self(to_i64(value, concat!(stringify!($name), "::mul_ratio")))
            }

            /// Lossy conversion for statistics and charts. Never feed the
            /// result back into accounting.
            pub fn to_f64(self) -> f64 {
                self.0 as f64 / SCALE as f64
            }
        }

        impl Add for $name {
            type Output = Self;
            fn add(self, rhs: Self) -> Self {
                self.checked_add(rhs).expect(concat!(stringify!($name), " overflow in add"))
            }
        }

        impl AddAssign for $name {
            fn add_assign(&mut self, rhs: Self) {
                *self = *self + rhs;
            }
        }

        impl Sub for $name {
            type Output = Self;
            fn sub(self, rhs: Self) -> Self {
                self.checked_sub(rhs).expect(concat!(stringify!($name), " overflow in sub"))
            }
        }

        impl SubAssign for $name {
            fn sub_assign(&mut self, rhs: Self) {
                *self = *self - rhs;
            }
        }

        impl Neg for $name {
            type Output = Self;
            fn neg(self) -> Self {
                Self(self.0.checked_neg().expect(concat!(stringify!($name), " overflow in neg")))
            }
        }

        impl std::iter::Sum for $name {
            fn sum<I: Iterator<Item = Self>>(iter: I) -> Self {
                iter.fold(Self::ZERO, Add::add)
            }
        }

        impl FromStr for $name {
            type Err = ParseFixedError;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                parse_fixed(s).map(Self)
            }
        }

        impl TryFrom<String> for $name {
            type Error = ParseFixedError;
            fn try_from(s: String) -> Result<Self, Self::Error> {
                s.parse()
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> String {
                value.to_string()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                format_fixed(self.0, f)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!(stringify!($name), "({})"), self)
            }
        }
    };
}

fixed_type!(
    /// The price of one unit of an instrument, in the quote currency.
    Price
);
fixed_type!(
    /// An amount of an instrument. Negative quantities represent short
    /// positions; order quantities are always positive.
    Qty
);
fixed_type!(
    /// An amount of the quote currency: cash, notional value, fees, P&L.
    Money
);

impl Mul<Qty> for Price {
    type Output = Money;

    /// Notional value, rounded half-to-even to 10^-8.
    fn mul(self, qty: Qty) -> Money {
        let value = div_round_half_even(i128::from(self.0) * i128::from(qty.0), i128::from(SCALE));
        Money(to_i64(value, "Price * Qty"))
    }
}

impl Mul<Price> for Qty {
    type Output = Money;

    fn mul(self, price: Price) -> Money {
        price * self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Price {
        s.parse().unwrap()
    }

    fn q(s: &str) -> Qty {
        s.parse().unwrap()
    }

    #[test]
    fn parses_exactly() {
        assert_eq!(p("1").raw(), 100_000_000);
        assert_eq!(p("1.5").raw(), 150_000_000);
        assert_eq!(p("0.00000001").raw(), 1);
        assert_eq!(p("-42.25").raw(), -4_225_000_000);
        assert_eq!(p(".5").raw(), 50_000_000);
        assert_eq!(p("7.").raw(), 700_000_000);
        assert_eq!(p("+3").raw(), 300_000_000);
        assert_eq!(p("  12.3  ").raw(), 1_230_000_000);
    }

    #[test]
    fn accepts_trailing_zero_padding_but_not_extra_precision() {
        assert_eq!(p("0.0100000000").raw(), 1_000_000);
        assert!(matches!(
            "0.000000001".parse::<Price>(),
            Err(ParseFixedError::TooPrecise(_))
        ));
    }

    #[test]
    fn rejects_garbage() {
        for bad in ["", ".", "-", "abc", "1.2.3", "1e5", "1,000", "--1", "0x10"] {
            assert!(bad.parse::<Price>().is_err(), "{bad:?} should not parse");
        }
    }

    #[test]
    fn rejects_out_of_range() {
        assert!(matches!(
            "92233720368.54775808".parse::<Price>(),
            Err(ParseFixedError::Overflow(_))
        ));
        assert!(matches!(
            "99999999999999999999999".parse::<Price>(),
            Err(ParseFixedError::Overflow(_))
        ));
        assert_eq!(p("92233720368.54775807").raw(), i64::MAX);
    }

    #[test]
    fn displays_without_trailing_zeros() {
        assert_eq!(p("1").to_string(), "1");
        assert_eq!(p("1.50").to_string(), "1.5");
        assert_eq!(p("-0.5").to_string(), "-0.5");
        assert_eq!(p("0.00000001").to_string(), "0.00000001");
        assert_eq!(
            Price::from_raw(i64::MIN).to_string(),
            "-92233720368.54775808"
        );
        assert_eq!(format!("{:?}", q("2.5")), "Qty(2.5)");
    }

    #[test]
    fn decimal_arithmetic_is_exact() {
        let tenth: Money = "0.1".parse().unwrap();
        let fifth: Money = "0.2".parse().unwrap();
        assert_eq!(tenth + fifth, "0.3".parse().unwrap());
        let total: Money = std::iter::repeat_n(tenth, 1_000_000).sum();
        assert_eq!(total, Money::from_int(100_000).unwrap());
    }

    #[test]
    fn notional_rounds_half_to_even() {
        assert_eq!(p("64250.50") * q("0.002"), "128.501".parse().unwrap());
        // 0.00000001 * 0.5 = 0.000000005, exactly halfway: rounds to the even 0.
        assert_eq!(p("0.00000001") * q("0.5"), Money::ZERO);
        // 0.00000003 * 0.5 = 0.000000015: rounds to the even 0.00000002.
        assert_eq!(p("0.00000003") * q("0.5"), Money::from_raw(2));
        assert_eq!(p("-0.00000003") * q("0.5"), Money::from_raw(-2));
        assert_eq!(q("3") * p("1.25"), p("1.25") * q("3"));
    }

    #[test]
    fn mul_ratio_applies_fee_rates() {
        let notional: Money = "10000".parse().unwrap();
        // 7.5 basis points = 75 / 100_000.
        assert_eq!(notional.mul_ratio(75, 100_000), "7.5".parse().unwrap());
        assert_eq!(notional.mul_ratio(-1, -4), "2500".parse().unwrap());
    }

    #[test]
    #[should_panic(expected = "overflow")]
    fn overflow_panics_instead_of_wrapping() {
        let _ = Money::from_raw(i64::MAX) + Money::from_raw(1);
    }

    #[test]
    fn serializes_as_exact_strings() {
        let json = serde_json::to_string(&p("64250.5")).unwrap();
        assert_eq!(json, "\"64250.5\"");
        let back: Price = serde_json::from_str(&json).unwrap();
        assert_eq!(back, p("64250.5"));
        assert!(serde_json::from_str::<Price>("\"1e3\"").is_err());
    }
}
