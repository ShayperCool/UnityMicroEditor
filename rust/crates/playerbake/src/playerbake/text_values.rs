//! The scalars and hexadecimal runs of the Editor's text serialization, as
//! the fields of a type tree read them.
use super::{typetree::Val, unsupported};
use anyhow::Result;

/// A decimal number as the Editor writes one. Other spellings, which the
/// engine may read differently, are refused.
pub(super) fn decimal(text: &str, fraction: bool) -> bool {
    let digits = |text: &str| !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit());
    let text = text.strip_prefix('-').unwrap_or(text);
    if !fraction {
        return digits(text);
    }
    let (mantissa, exponent) = match text.split_once(['e', 'E']) {
        Some((mantissa, exponent)) => (mantissa, Some(exponent)),
        None => (text, None),
    };
    // Older Editors wrote a fraction below one without its zero (`.001`).
    let whole = match mantissa.split_once('.') {
        Some(("", part)) => digits(part).then_some("0"),
        Some((whole, part)) => digits(part).then_some(whole),
        None => Some(mantissa),
    };
    whole.is_some_and(digits)
        && exponent
            .is_none_or(|exponent| digits(exponent.strip_prefix(['+', '-']).unwrap_or(exponent)))
}

pub(super) fn scalar(type_name: &str, source: &str, path: &str) -> Result<Val> {
    let refused = || unsupported(format!("{path} is not a plain number: {source:?}"));
    Ok(match type_name {
        // The Editor writes an infinite single as a word.
        "float" if source == "Infinity" || source == "inf" => Val::F(f32::INFINITY),
        "float" if source == "-Infinity" || source == "-inf" => Val::F(f32::NEG_INFINITY),
        "float" => {
            let number = source
                .parse::<f32>()
                .ok()
                .filter(|number| number.is_finite());
            match number {
                Some(number) if decimal(source, true) => Val::F(number),
                _ => return Err(refused()),
            }
        }
        "double" => {
            let number = source
                .parse::<f64>()
                .ok()
                .filter(|number| number.is_finite());
            match number {
                Some(number) if decimal(source, true) => Val::D(number),
                _ => return Err(refused()),
            }
        }
        "bool" => match source {
            "0" => Val::Bool(false),
            "1" => Val::Bool(true),
            _ => return Err(refused()),
        },
        "UInt8" | "UInt16" | "unsigned short" | "unsigned int" | "UInt64"
        | "unsigned long long" => match source.parse::<u64>() {
            Ok(number) if decimal(source, false) => Val::U(number),
            _ => return Err(refused()),
        },
        "SInt8" | "SInt16" | "short" | "int" | "Type*" | "SInt64" | "long long" => {
            match source.parse::<i64>() {
                Ok(number) if decimal(source, false) => Val::I(number),
                _ => return Err(refused()),
            }
        }
        other => return Err(unsupported(format!("{path} has the type {other}"))),
    })
}

/// The Editor writes a sequence of integers as one run of hexadecimal
/// digits, each item in little-endian byte order; a sequence of unsigned
/// 32-bit integers ends with `u`.
pub(super) fn hex_items(type_name: &str, dump: &str, path: &str) -> Result<Vec<Val>> {
    let (size, signed, suffix) = match type_name {
        "UInt8" => (1, false, ""),
        "SInt8" | "char" => (1, true, ""),
        "UInt16" | "unsigned short" => (2, false, ""),
        "SInt16" | "short" => (2, true, ""),
        "unsigned int" => (4, false, "u"),
        "int" => (4, true, ""),
        other => {
            return Err(unsupported(format!(
                "{path} is a hexadecimal run of {other} items"
            )));
        }
    };
    let digits = dump.strip_suffix(suffix).unwrap_or(dump).as_bytes();
    let (items, rest) = digits.as_chunks::<2>();
    if !rest.is_empty()
        || !items.len().is_multiple_of(size)
        || !digits.iter().all(u8::is_ascii_hexdigit)
    {
        return Err(unsupported(format!(
            "{path} is not a hexadecimal run of {type_name} items"
        )));
    }
    let bytes: Vec<u8> = items
        .iter()
        .map(|pair| u8::from_str_radix(&String::from_utf8_lossy(pair), 16).unwrap_or_default())
        .collect();
    Ok(bytes
        .chunks(size)
        .map(|item| {
            let mut wide = [0u8; 8];
            wide[..size].copy_from_slice(item);
            let number = u64::from_le_bytes(wide);
            if signed {
                let shift = 64 - 8 * size as u32;
                Val::I(((number << shift) as i64) >> shift)
            } else {
                Val::U(number)
            }
        })
        .collect())
}
