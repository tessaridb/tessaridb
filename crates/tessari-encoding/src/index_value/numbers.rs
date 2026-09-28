//! How a number is written into an index key so bytes sort as numbers do.

use super::{
    DIGITS_END, DIGITS_END_NEGATIVE, NUMBER_NEGATIVE, NUMBER_NEGATIVE_INFINITY,
    NUMBER_NOT_A_NUMBER, NUMBER_POSITIVE, NUMBER_POSITIVE_INFINITY, NUMBER_ZERO,
};
use crate::error::Result;
use crate::order::{KeyReader, KeyWriter};
use rust_decimal::Decimal;
use tessari_types::Number;

/// Encode a number in the form its comparison reduces to.
///
/// The digits come from [`Number::as_decimal`] whenever a decimal can hold the
/// number, because that is the form the ordering itself uses. Taking a float's
/// own digits instead would agree everywhere obvious and disagree exactly where
/// it matters: a float too small for a decimal compares **equal to zero**, and
/// its own digits would place it just above.
pub(crate) fn put_number(writer: &mut KeyWriter, number: &Number) {
    if let Number::Float(value) = number {
        if value.is_nan() {
            writer.put_u8(NUMBER_NOT_A_NUMBER);
            return;
        }
        if *value == f64::INFINITY {
            writer.put_u8(NUMBER_POSITIVE_INFINITY);
            return;
        }
        if *value == f64::NEG_INFINITY {
            writer.put_u8(NUMBER_NEGATIVE_INFINITY);
            return;
        }
    }

    let form = match number.as_decimal() {
        Some(decimal) => decimal_form(decimal),
        // A finite float beyond decimal range. Its magnitude exceeds anything a
        // decimal holds, so encoding its own digits keeps it above every decimal
        // of the same sign — which is the order the comparison declares.
        None => match number {
            Number::Float(value) => float_form(*value),
            _ => None,
        },
    };

    let Some(Magnitude {
        negative,
        exponent,
        digits,
    }) = form
    else {
        writer.put_u8(NUMBER_ZERO);
        return;
    };

    let mut exponent_bytes = exponent.to_be_bytes();
    // Sign-flip so negative exponents sort below positive ones.
    exponent_bytes[0] ^= 0x80;

    if negative {
        // Every byte is complemented, so a larger magnitude sorts lower — which
        // is what "more negative" means.
        writer.put_u8(NUMBER_NEGATIVE);
        for byte in &mut exponent_bytes {
            *byte = !*byte;
        }
        writer.put_fixed(&exponent_bytes);
        for digit in &digits {
            writer.put_u8(!*digit);
        }
        writer.put_u8(DIGITS_END_NEGATIVE);
    } else {
        writer.put_u8(NUMBER_POSITIVE);
        writer.put_fixed(&exponent_bytes);
        writer.put_fixed(&digits);
        writer.put_u8(DIGITS_END);
    }
}

pub(crate) fn skip_number(reader: &mut KeyReader<'_>) -> Result<()> {
    let class = reader.take_u8()?;
    let terminator = match class {
        NUMBER_NEGATIVE => DIGITS_END_NEGATIVE,
        NUMBER_POSITIVE => DIGITS_END,
        _ => return Ok(()),
    };
    reader.take_exact(4)?;
    loop {
        if reader.take_u8()? == terminator {
            return Ok(());
        }
    }
}

/// A finite non-zero number as sign, decimal exponent and significant digits.
///
/// The value is `0.<digits> × 10^exponent`, which is the form in which
/// comparing the exponent and then the digit string is the same as comparing the
/// numbers.
pub(crate) struct Magnitude {
    pub(crate) negative: bool,
    pub(crate) exponent: i32,
    pub(crate) digits: Vec<u8>,
}

pub(crate) fn decimal_form(decimal: Decimal) -> Option<Magnitude> {
    let mantissa = decimal.mantissa();
    if mantissa == 0 {
        return None;
    }
    let digits = mantissa.unsigned_abs().to_string().into_bytes();
    let exponent = digit_count(digits.len()).saturating_sub(digit_count_of(decimal.scale()));
    Some(Magnitude {
        negative: mantissa < 0,
        exponent,
        digits: strip_trailing_zeros(digits),
    })
}

/// The digits of a float that no decimal can hold.
///
/// `{:e}` is the shortest form that round-trips, which is exactly the digit
/// string wanted here: `1.5e3` becomes `0.15 × 10^4`.
pub(crate) fn float_form(value: f64) -> Option<Magnitude> {
    if value == 0.0 {
        return None;
    }
    let formatted = format!("{value:e}");
    let (mantissa, exponent) = formatted.split_once('e')?;
    let negative = mantissa.starts_with('-');
    let digits: Vec<u8> = mantissa
        .bytes()
        .filter(u8::is_ascii_digit)
        .collect::<Vec<u8>>();
    let exponent = exponent.parse::<i32>().ok()?.saturating_add(1);
    Some(Magnitude {
        negative,
        exponent,
        digits: strip_trailing_zeros(digits),
    })
}

/// Trailing zeros carry no value in `0.<digits>` form, and dropping them is what
/// makes `1.50` and `1.5` one set of bytes.
pub(crate) fn strip_trailing_zeros(mut digits: Vec<u8>) -> Vec<u8> {
    while digits.len() > 1 && digits.last() == Some(&b'0') {
        digits.pop();
    }
    digits
}

/// A digit string from any of the three numeric kinds is under forty bytes, and
/// a scale is under thirty, so saturating keeps these total without a branch
/// that cannot be reached.
pub(crate) fn digit_count(len: usize) -> i32 {
    i32::try_from(len).unwrap_or(i32::MAX)
}

pub(crate) fn digit_count_of(scale: u32) -> i32 {
    i32::try_from(scale).unwrap_or(i32::MAX)
}
