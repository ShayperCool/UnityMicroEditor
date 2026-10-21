//! Integer and float parsing with base prefixes and underscores, and the error wording the tool reports.
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NumError {
    function: &'static str,
    text: String,
    range: bool,
}
impl fmt::Display for NumError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let reason = if self.range {
            "value out of range"
        } else {
            "invalid syntax"
        };
        write!(f, "parsing {:?}: {reason}", self.text)
    }
}
impl std::error::Error for NumError {}
fn error(function: &'static str, text: &str, range: bool) -> NumError {
    NumError {
        function,
        text: text.into(),
        range,
    }
}

/// Underscores may only separate digits (or follow a base prefix).
fn underscore_ok(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut saw = b'^';
    let mut i = usize::from(matches!(bytes.first(), Some(b'+' | b'-')));
    let mut hex = false;
    if bytes.len() - i >= 2
        && bytes[i] == b'0'
        && matches!(bytes[i + 1].to_ascii_lowercase(), b'b' | b'o' | b'x')
    {
        hex = bytes[i + 1].eq_ignore_ascii_case(&b'x');
        i += 2;
        saw = b'0';
    }
    for &b in &bytes[i..] {
        if b.is_ascii_digit() || hex && b.is_ascii_hexdigit() {
            saw = b'0';
        } else if b == b'_' {
            if saw != b'0' {
                return false;
            }
            saw = b'_';
        } else {
            if saw == b'_' {
                return false;
            }
            saw = b'!';
        }
    }
    saw != b'_'
}

/// Parses an unsigned 64-bit integer; base 0 reads prefixes and underscores.
pub fn parse_uint(text: &str, base: u32) -> Result<u64, NumError> {
    const NAME: &str = "ParseUint";
    if text.is_empty() {
        return Err(error(NAME, text, false));
    }
    let bytes = text.as_bytes();
    let (radix, body) = if base != 0 {
        (base, text)
    } else if bytes[0] == b'0' {
        match bytes.get(1).map(u8::to_ascii_lowercase) {
            Some(b'b') if bytes.len() >= 3 => (2, &text[2..]),
            Some(b'o') if bytes.len() >= 3 => (8, &text[2..]),
            Some(b'x') if bytes.len() >= 3 => (16, &text[2..]),
            _ => (8, &text[1..]),
        }
    } else {
        (10, text)
    };
    let mut value: u64 = 0;
    let mut overflow = false;
    let mut underscores = false;
    for b in body.bytes() {
        if b == b'_' && base == 0 {
            underscores = true;
            continue;
        }
        let Some(digit) = (b as char).to_digit(radix) else {
            return Err(error(NAME, text, false));
        };
        match value
            .checked_mul(radix as u64)
            .and_then(|v| v.checked_add(digit as u64))
        {
            Some(next) => value = next,
            None => overflow = true,
        }
    }
    if underscores && !underscore_ok(text) {
        return Err(error(NAME, text, false));
    }
    if overflow {
        return Err(error(NAME, text, true));
    }
    Ok(value)
}

/// Parses a signed 64-bit integer with an optional sign.
pub fn parse_int(text: &str, base: u32) -> Result<i64, NumError> {
    const NAME: &str = "ParseInt";
    if text.is_empty() {
        return Err(error(NAME, text, false));
    }
    let (negative, body) = match text.as_bytes()[0] {
        b'-' => (true, &text[1..]),
        b'+' => (false, &text[1..]),
        _ => (false, text),
    };
    let magnitude = parse_uint(body, base).map_err(|e| error(NAME, text, e.range))?;
    if negative && magnitude <= 1 << 63 {
        return Ok((magnitude as i64).wrapping_neg());
    }
    if !negative && magnitude < 1 << 63 {
        return Ok(magnitude as i64);
    }
    Err(error(NAME, text, true))
}

/// Parses a base-10 integer, ignoring errors: overflow saturates, bad syntax is zero.
pub fn atoi_lossy(text: &str) -> i64 {
    match parse_int(text, 10) {
        Ok(value) => value,
        Err(e) if e.range => {
            if text.starts_with('-') {
                i64::MIN
            } else {
                i64::MAX
            }
        }
        Err(_) => 0,
    }
}
pub fn atoi(text: &str) -> Result<i64, NumError> {
    parse_int(text, 10).map_err(|e| error("Atoi", text, e.range))
}

fn hex_float(text: &str, negative: bool, body: &str) -> Result<f64, NumError> {
    const NAME: &str = "ParseFloat";
    let bad = || error(NAME, text, false);
    let lower = body.to_ascii_lowercase();
    let (mantissa, exponent) = lower.split_once('p').ok_or_else(bad)?;
    if exponent.contains('_') {
        return Err(bad());
    }
    let exponent: i64 = exponent.parse().map_err(|_| bad())?;
    let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let (mut value, mut shift, mut digits, mut sticky) = (0u64, 0i64, 0, false);
    for (part, fractional) in [(whole, false), (fraction, true)] {
        for b in part.bytes().filter(|b| *b != b'_') {
            let digit = (b as char).to_digit(16).ok_or_else(bad)? as u64;
            digits += 1;
            if value >> 60 == 0 {
                value = value << 4 | digit;
                if fractional {
                    shift -= 4;
                }
            } else {
                sticky |= digit != 0;
                if !fractional {
                    shift += 4;
                }
            }
        }
    }
    if digits == 0 {
        return Err(bad());
    }
    // Keep a sticky bit so the single rounding to f64 is exact.
    let magnitude = (value | u64::from(sticky && value & 1 == 0)) as f64;
    let result = magnitude
        * 2f64.powi((shift + exponent).clamp(-4000, 4000) as i32 / 2)
        * 2f64.powi(
            (shift + exponent).clamp(-4000, 4000) as i32
                - (shift + exponent).clamp(-4000, 4000) as i32 / 2,
        );
    if result.is_infinite() {
        return Err(error(NAME, text, true));
    }
    Ok(if negative { -result } else { result })
}

/// Parses an f64. `inf`/`nan` spellings succeed with non-finite values;
/// overflow is a range error.
pub fn parse_float(text: &str) -> Result<f64, NumError> {
    const NAME: &str = "ParseFloat";
    let bad = || error(NAME, text, false);
    let bytes = text.as_bytes();
    let signed = matches!(bytes.first(), Some(b'+' | b'-'));
    let negative = bytes.first() == Some(&b'-');
    let body = &text[usize::from(signed)..];
    let lower = body.to_ascii_lowercase();
    if lower == "inf" || lower == "infinity" {
        return Ok(if negative {
            f64::NEG_INFINITY
        } else {
            f64::INFINITY
        });
    }
    if !signed && lower == "nan" {
        return Ok(f64::NAN);
    }
    if text.contains('_') && !underscore_ok(text) {
        return Err(bad());
    }
    if let Some(hex) = lower.strip_prefix("0x") {
        return hex_float(text, negative, hex);
    }
    let mut i = 0;
    let digits = body.as_bytes();
    let (mut count, mut dot) = (0, false);
    while let Some(&b) = digits.get(i) {
        match b {
            b'0'..=b'9' => count += 1,
            b'_' => {}
            b'.' if !dot => dot = true,
            _ => break,
        }
        i += 1;
    }
    if count == 0 {
        return Err(bad());
    }
    if matches!(digits.get(i), Some(b'e' | b'E')) {
        i += 1;
        if matches!(digits.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        let start = i;
        while digits
            .get(i)
            .is_some_and(|b| b.is_ascii_digit() || *b == b'_')
        {
            i += 1;
        }
        if !digits[start..i].iter().any(u8::is_ascii_digit) {
            return Err(bad());
        }
    }
    if i != digits.len() {
        return Err(bad());
    }
    let value: f64 = text.replace('_', "").parse().map_err(|_| bad())?;
    if value.is_infinite() {
        return Err(error(NAME, text, true));
    }
    Ok(value)
}

/// Parses an f32, as used for finite pose checks.
pub fn parse_float32(text: &str) -> Result<f32, NumError> {
    let wide = parse_float(text)?;
    if !wide.is_finite() {
        return Ok(wide as f32);
    }
    let cleaned = text.replace('_', "");
    let narrow: f32 = cleaned.parse().unwrap_or(wide as f32);
    if narrow.is_infinite() {
        return Err(error("ParseFloat", text, true));
    }
    Ok(narrow)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn integers_and_floats_parse() {
        assert_eq!(parse_int("0x10", 0), Ok(16));
        assert_eq!(parse_int("-0b11", 0), Ok(-3));
        assert_eq!(parse_int("012", 0), Ok(10));
        assert_eq!(parse_int("0", 0), Ok(0));
        assert!(parse_int("08", 0).is_err() && parse_int("0x", 0).is_err());
        assert!(parse_int("0x+1", 0).is_err() && parse_int("++1", 0).is_err());
        assert_eq!(parse_int("1_000", 0), Ok(1000));
        assert!(parse_int("_1", 0).is_err() && parse_int("1_000", 10).is_err());
        assert_eq!(parse_int("-9223372036854775808", 10), Ok(i64::MIN));
        assert!(parse_int("9223372036854775808", 10).unwrap_err().range);
        assert_eq!(parse_uint("18446744073709551615", 10), Ok(u64::MAX));
        assert_eq!(parse_float("1_0"), Ok(10.0));
        assert_eq!(parse_float("+.5"), Ok(0.5));
        assert_eq!(parse_float("1."), Ok(1.0));
        assert_eq!(parse_float("0x1p-2"), Ok(0.25));
        assert_eq!(parse_float("0x1.8p3"), Ok(12.0));
        assert!(parse_float("1e400").unwrap_err().range);
        assert!(parse_float("Infinity").unwrap().is_infinite());
        assert!(parse_float("nan").unwrap().is_nan());
        for bad in ["", " 1", "0x10", "5_.5", "5._", "1_e1", "1e", "+nan", "."] {
            assert!(parse_float(bad).is_err(), "{bad}");
        }
        assert_eq!(
            parse_int("x", 10).unwrap_err().to_string(),
            "parsing \"x\": invalid syntax"
        );
    }
}
