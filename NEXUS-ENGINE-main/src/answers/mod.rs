//! Instant answer cards computed locally and deterministically, no
//! external API call needed — the two kinds of query where a fixed
//! computation genuinely is the whole answer: arithmetic and unit
//! conversion.
//!
//! **Scoped down from the feature request in one specific way, stated
//! plainly:** currency conversion ("Convert 100 USD to EUR") needs a
//! live, continuously-updated exchange rate feed — there's no way to
//! answer that correctly with a static table, and shipping a
//! hardcoded/stale rate table would silently give wrong answers that
//! *look* authoritative, which is worse than not answering. This module
//! implements physical unit conversion (length, mass, volume,
//! temperature — all fixed, unchanging conversion factors) and
//! arithmetic, not currency. A currency card would need
//! `crate::ai::client` or a dedicated FX-rate API call wired in
//! separately, with the response clearly timestamped as "as of [time],"
//! which is a different (and honestly, fairly small) follow-up task.
//!
//! Knowledge-panel-style factual cards ("what's the capital of France")
//! are **not** implemented here at all — that needs either a curated
//! local knowledge base (a real, separate content-curation effort, not
//! a code change) or a call out to `crate::ai::summarize` over indexed
//! content, which already exists as a separate, general-purpose
//! capability rather than needing a bespoke "instant answer" path.

use std::fmt;

#[derive(Debug, Clone, PartialEq)]
pub enum InstantAnswer {
    Calculation { expression: String, result: f64 },
    UnitConversion { from: String, to: String, input: f64, result: f64 },
}

impl fmt::Display for InstantAnswer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InstantAnswer::Calculation { expression, result } => {
                write!(f, "{expression} = {result}")
            }
            InstantAnswer::UnitConversion { from, to, input, result } => {
                write!(f, "{input} {from} = {result} {to}")
            }
        }
    }
}

/// Tries to answer `query` as an instant-answer card. Returns `None` if
/// the query doesn't look like arithmetic or a unit conversion — callers
/// should fall through to normal search in that case, this is meant as
/// a fast-path check tried before/alongside a real search, not a
/// replacement for one.
pub fn try_answer(query: &str) -> Option<InstantAnswer> {
    if let Some(conversion) = try_unit_conversion(query) {
        return Some(conversion);
    }
    try_calculation(query)
}

fn try_calculation(query: &str) -> Option<InstantAnswer> {
    let trimmed = query.trim();
    // Only attempt this for queries that are plausibly "just math" —
    // require at least one operator and only expression-shaped
    // characters, so an ordinary text query never gets misinterpreted.
    let looks_like_math = trimmed
        .chars()
        .all(|c| c.is_ascii_digit() || "+-*/^(). ".contains(c))
        && trimmed.chars().any(|c| "+-*/^".contains(c))
        && trimmed.chars().any(|c| c.is_ascii_digit());
    if !looks_like_math {
        return None;
    }
    let result = evaluate_expression(trimmed)?;
    Some(InstantAnswer::Calculation {
        expression: trimmed.to_string(),
        result,
    })
}

/// A small recursive-descent parser/evaluator for `+ - * / ^` and
/// parentheses over floating-point numbers — standard operator
/// precedence (`^` binds tightest and right-associates, then `* /`,
/// then `+ -`), no external expression-evaluation crate needed for
/// something this contained.
fn evaluate_expression(expr: &str) -> Option<f64> {
    let tokens = tokenize_expression(expr)?;
    let mut pos = 0;
    let result = parse_add_sub(&tokens, &mut pos)?;
    if pos != tokens.len() {
        return None; // trailing garbage the parser couldn't consume
    }
    Some(result)
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Num(f64),
    Plus,
    Minus,
    Star,
    Slash,
    Caret,
    LParen,
    RParen,
}

fn tokenize_expression(expr: &str) -> Option<Vec<Tok>> {
    let mut tokens = Vec::new();
    let chars: Vec<char> = expr.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            ' ' => i += 1,
            '+' => {
                tokens.push(Tok::Plus);
                i += 1;
            }
            '-' => {
                tokens.push(Tok::Minus);
                i += 1;
            }
            '*' => {
                tokens.push(Tok::Star);
                i += 1;
            }
            '/' => {
                tokens.push(Tok::Slash);
                i += 1;
            }
            '^' => {
                tokens.push(Tok::Caret);
                i += 1;
            }
            '(' => {
                tokens.push(Tok::LParen);
                i += 1;
            }
            ')' => {
                tokens.push(Tok::RParen);
                i += 1;
            }
            c if c.is_ascii_digit() || c == '.' => {
                let start = i;
                while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.') {
                    i += 1;
                }
                let text: String = chars[start..i].iter().collect();
                tokens.push(Tok::Num(text.parse().ok()?));
            }
            _ => return None,
        }
    }
    Some(tokens)
}

fn parse_add_sub(tokens: &[Tok], pos: &mut usize) -> Option<f64> {
    let mut value = parse_mul_div(tokens, pos)?;
    loop {
        match tokens.get(*pos) {
            Some(Tok::Plus) => {
                *pos += 1;
                value += parse_mul_div(tokens, pos)?;
            }
            Some(Tok::Minus) => {
                *pos += 1;
                value -= parse_mul_div(tokens, pos)?;
            }
            _ => break,
        }
    }
    Some(value)
}

fn parse_mul_div(tokens: &[Tok], pos: &mut usize) -> Option<f64> {
    let mut value = parse_power(tokens, pos)?;
    loop {
        match tokens.get(*pos) {
            Some(Tok::Star) => {
                *pos += 1;
                value *= parse_power(tokens, pos)?;
            }
            Some(Tok::Slash) => {
                *pos += 1;
                let divisor = parse_power(tokens, pos)?;
                if divisor == 0.0 {
                    return None;
                }
                value /= divisor;
            }
            _ => break,
        }
    }
    Some(value)
}

fn parse_power(tokens: &[Tok], pos: &mut usize) -> Option<f64> {
    let base = parse_unary(tokens, pos)?;
    if matches!(tokens.get(*pos), Some(Tok::Caret)) {
        *pos += 1;
        let exponent = parse_power(tokens, pos)?; // right-associative
        return Some(base.powf(exponent));
    }
    Some(base)
}

fn parse_unary(tokens: &[Tok], pos: &mut usize) -> Option<f64> {
    if matches!(tokens.get(*pos), Some(Tok::Minus)) {
        *pos += 1;
        return Some(-parse_unary(tokens, pos)?);
    }
    parse_atom(tokens, pos)
}

fn parse_atom(tokens: &[Tok], pos: &mut usize) -> Option<f64> {
    match tokens.get(*pos) {
        Some(Tok::Num(n)) => {
            *pos += 1;
            Some(*n)
        }
        Some(Tok::LParen) => {
            *pos += 1;
            let value = parse_add_sub(tokens, pos)?;
            if matches!(tokens.get(*pos), Some(Tok::RParen)) {
                *pos += 1;
                Some(value)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// A physical unit and the family (length/mass/volume/temperature) it
/// belongs to — conversions only make sense within the same family.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Family {
    Length,
    Mass,
    Volume,
    Temperature,
}

/// `(unit name as it appears in a query, family, factor to convert TO
/// the family's base unit)`. Temperature is handled specially (affine,
/// not just a scale factor) — see `convert_temperature`.
const UNITS: &[(&str, Family, f64)] = &[
    // Length, base unit: meters.
    ("mm", Family::Length, 0.001),
    ("cm", Family::Length, 0.01),
    ("m", Family::Length, 1.0),
    ("km", Family::Length, 1000.0),
    ("in", Family::Length, 0.0254),
    ("inch", Family::Length, 0.0254),
    ("inches", Family::Length, 0.0254),
    ("ft", Family::Length, 0.3048),
    ("feet", Family::Length, 0.3048),
    ("foot", Family::Length, 0.3048),
    ("yd", Family::Length, 0.9144),
    ("yard", Family::Length, 0.9144),
    ("mi", Family::Length, 1609.344),
    ("mile", Family::Length, 1609.344),
    ("miles", Family::Length, 1609.344),
    // Mass, base unit: grams.
    ("mg", Family::Mass, 0.001),
    ("g", Family::Mass, 1.0),
    ("kg", Family::Mass, 1000.0),
    ("oz", Family::Mass, 28.349523125),
    ("ounce", Family::Mass, 28.349523125),
    ("lb", Family::Mass, 453.59237),
    ("lbs", Family::Mass, 453.59237),
    ("pound", Family::Mass, 453.59237),
    ("pounds", Family::Mass, 453.59237),
    // Volume, base unit: liters.
    ("ml", Family::Volume, 0.001),
    ("l", Family::Volume, 1.0),
    ("liter", Family::Volume, 1.0),
    ("liters", Family::Volume, 1.0),
    ("gal", Family::Volume, 3.785411784),
    ("gallon", Family::Volume, 3.785411784),
    ("gallons", Family::Volume, 3.785411784),
    ("cup", Family::Volume, 0.2365882365),
    ("cups", Family::Volume, 0.2365882365),
    // Temperature: handled via `convert_temperature`, factor unused.
    ("c", Family::Temperature, 0.0),
    ("celsius", Family::Temperature, 0.0),
    ("f", Family::Temperature, 0.0),
    ("fahrenheit", Family::Temperature, 0.0),
    ("k", Family::Temperature, 0.0),
    ("kelvin", Family::Temperature, 0.0),
];

fn find_unit(name: &str) -> Option<&(&'static str, Family, f64)> {
    let lower = name.to_lowercase();
    UNITS.iter().find(|(n, _, _)| *n == lower)
}

fn try_unit_conversion(query: &str) -> Option<InstantAnswer> {
    let lower = query.to_lowercase();
    let lower = lower.replace(" to ", " ").replace("convert ", "");
    let parts: Vec<&str> = lower.split_whitespace().collect();
    // Expected shape after normalization: ["<number>", "<from-unit>", "<to-unit>"]
    if parts.len() != 3 {
        return None;
    }
    let input: f64 = parts[0].parse().ok()?;
    let (from_name, from_family, from_factor) = find_unit(parts[1])?;
    let (to_name, to_family, to_factor) = find_unit(parts[2])?;
    if from_family != to_family {
        return None;
    }

    let result = if *from_family == Family::Temperature {
        convert_temperature(input, parts[1], parts[2])?
    } else {
        let base_value = input * from_factor;
        base_value / to_factor
    };

    Some(InstantAnswer::UnitConversion {
        from: from_name.to_string(),
        to: to_name.to_string(),
        input,
        result: round_to(result, 4),
    })
}

fn convert_temperature(value: f64, from: &str, to: &str) -> Option<f64> {
    let celsius = match normalize_temp_unit(from)? {
        'c' => value,
        'f' => (value - 32.0) * 5.0 / 9.0,
        'k' => value - 273.15,
        _ => return None,
    };
    let result = match normalize_temp_unit(to)? {
        'c' => celsius,
        'f' => celsius * 9.0 / 5.0 + 32.0,
        'k' => celsius + 273.15,
        _ => return None,
    };
    Some(result)
}

fn normalize_temp_unit(name: &str) -> Option<char> {
    match name {
        "c" | "celsius" => Some('c'),
        "f" | "fahrenheit" => Some('f'),
        "k" | "kelvin" => Some('k'),
        _ => None,
    }
}

fn round_to(value: f64, decimals: u32) -> f64 {
    let factor = 10f64.powi(decimals as i32);
    (value * factor).round() / factor
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evaluates_basic_arithmetic_with_precedence() {
        assert_eq!(evaluate_expression("2 + 3 * 4"), Some(14.0));
        assert_eq!(evaluate_expression("(2 + 3) * 4"), Some(20.0));
        assert_eq!(evaluate_expression("2 ^ 3 ^ 2"), Some(512.0)); // right-associative: 2^(3^2)
        assert_eq!(evaluate_expression("-5 + 3"), Some(-2.0));
    }

    #[test]
    fn division_by_zero_returns_none_rather_than_infinity() {
        assert_eq!(evaluate_expression("1 / 0"), None);
    }

    #[test]
    fn try_answer_recognizes_a_math_query() {
        let answer = try_answer("12 * (3 + 4)").unwrap();
        assert_eq!(
            answer,
            InstantAnswer::Calculation {
                expression: "12 * (3 + 4)".to_string(),
                result: 84.0
            }
        );
    }

    #[test]
    fn try_answer_returns_none_for_ordinary_text_queries() {
        assert_eq!(try_answer("rust ownership guide"), None);
    }

    #[test]
    fn converts_length_units() {
        let answer = try_answer("10 km to miles").unwrap();
        match answer {
            InstantAnswer::UnitConversion { result, .. } => {
                assert!((result - 6.2137).abs() < 0.001);
            }
            _ => panic!("expected a unit conversion"),
        }
    }

    #[test]
    fn converts_temperature_with_affine_formula() {
        let answer = try_answer("100 celsius to fahrenheit").unwrap();
        match answer {
            InstantAnswer::UnitConversion { result, .. } => {
                assert!((result - 212.0).abs() < 0.001);
            }
            _ => panic!("expected a unit conversion"),
        }
    }

    #[test]
    fn mismatched_unit_families_are_not_converted() {
        assert_eq!(try_answer("10 km to kg"), None);
    }

    #[test]
    fn currency_conversion_is_explicitly_not_supported() {
        // "USD"/"EUR" aren't in the physical-unit table at all, so this
        // correctly falls through to None (search) rather than silently
        // producing a wrong/stale exchange rate — see the module doc
        // comment for why that's the intended behavior, not a bug.
        assert_eq!(try_answer("100 usd to eur"), None);
    }
}
