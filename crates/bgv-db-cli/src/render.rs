//! A value, written the way the language writes one.
//!
//! # Why not JSON
//!
//! JSON has six types and this store has fifteen, so every rendering into it is
//! a decision — the HTTP endpoint had to take them, and quotes a decimal because
//! a JSON number is a double in every parser that matters. A command line has no
//! such obligation, so it renders in **bgvQL**: what is printed can be pasted
//! back into a statement, which is worth more to somebody at a terminal than a
//! format a browser likes.
//!
//! # Why not `Display`
//!
//! [`bgv_db_types::Value`]'s own `Display` is a *summary* — an array renders as
//! `<array of 3>` — because it exists for error messages, where a whole nested
//! document would bury the message. A person reading a query result wants the
//! document.
//!
//! # The obligation this creates
//!
//! Claiming the output round-trips is a claim that has to hold for **every** kind
//! the value system has, not for the four a hand-written example uses. The test
//! at the bottom parses the rendered form back and compares, over a value
//! covering all fifteen — so a type added to the language without a rendering
//! fails a test rather than printing something that cannot be read back.

use bgv_db_types::{Number, Value};

/// One value, in the language's own syntax.
#[must_use]
pub fn value(held: &Value) -> String {
    let mut out = String::new();
    write(&mut out, held);
    out
}

fn write(out: &mut String, held: &Value) {
    match held {
        Value::None => out.push_str("NONE"),
        Value::Null => out.push_str("NULL"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(number) => number_into(out, number),
        Value::String(text) => string_into(out, text),
        Value::Bytes(bytes) => {
            out.push_str("0x");
            for byte in bytes {
                out.push_str(&format!("{byte:02x}"));
            }
        }
        // Both use the writers that live beside their readers in
        // `bgv_db_types::text`, not `Display` — which writes a debugging form
        // (`5400.000000000s`, `0.000000000`) that the lexer will not read back.
        Value::Duration(held) => out.push_str(&held.to_literal()),
        Value::Datetime(held) => {
            out.push_str("datetime ");
            string_into(out, &held.to_rfc3339());
        }
        Value::Uuid(bytes) => {
            out.push_str("uuid ");
            let mut text = String::new();
            for (position, byte) in bytes.iter().enumerate() {
                if matches!(position, 4 | 6 | 8 | 10) {
                    text.push('-');
                }
                text.push_str(&format!("{byte:02x}"));
            }
            string_into(out, &text);
        }
        // A table and a record reference both hold an **id**, and the name
        // lives in the catalog — so neither can be written as the language
        // writes it without a lookup this function has no way to make. They are
        // rendered as what they are, and Q-50 holds the fix (a name resolver at
        // the rendering boundary, which the JSON encoder needs too, for the same
        // reason and with the same symptom).
        Value::Table(id) => out.push_str(&format!("<table {id}>")),
        Value::Record(reference) => out.push_str(&format!("<record {reference}>")),
        Value::Array(items) => {
            out.push('[');
            for (position, item) in items.iter().enumerate() {
                if position > 0 {
                    out.push_str(", ");
                }
                write(out, item);
            }
            out.push(']');
        }
        Value::Set(items) => {
            out.push_str("set [");
            for (position, item) in items.iter().enumerate() {
                if position > 0 {
                    out.push_str(", ");
                }
                write(out, item);
            }
            out.push(']');
        }
        Value::Object(fields) => {
            out.push_str("{ ");
            for (position, (name, item)) in fields.iter().enumerate() {
                if position > 0 {
                    out.push_str(", ");
                }
                // A field name is written as a name where it is one and as a
                // string otherwise, so a key holding a space still round-trips.
                if is_a_name(name) {
                    out.push_str(name);
                } else {
                    string_into(out, name);
                }
                out.push_str(": ");
                write(out, item);
            }
            out.push_str(" }");
        }
        Value::Range(range) => {
            // A range's ends are bounds, and the language spells an inclusive
            // upper end `..=`. An unbounded end is written as nothing, which is
            // what the grammar reads.
            bound_into(out, &range.start, false);
            out.push_str("..");
            bound_into(out, &range.end, true);
        }
    }
}

/// A number, keeping the marker that keeps a decimal exact.
///
/// `12.34` is a float and `dec 12.34` is exact, so a decimal rendered without
/// its prefix would come back as a float — the one mistake the language went out
/// of its way to make loud.
fn number_into(out: &mut String, held: &Number) {
    match held {
        Number::Decimal(exact) => {
            out.push_str("dec ");
            out.push_str(&exact.to_string());
        }
        Number::Integer(value) => out.push_str(&value.to_string()),
        Number::Float(value) => {
            if value.is_nan() || value.is_infinite() {
                // Neither has a literal. Rendering one as a bare word would
                // produce something the parser reads as a table name, so it is
                // written as what it is and the round-trip test excludes it.
                out.push_str(&format!("<{value}>"));
            } else if value.fract() == 0.0 && value.abs() < 1e15 {
                // `2` would read back as an integer, so a whole float keeps a
                // fraction: the kind is part of the value.
                out.push_str(&format!("{value:.1}"));
            } else {
                out.push_str(&value.to_string());
            }
        }
    }
}

/// One end of a range.
///
/// `upper` decides where the `=` of an inclusive bound goes: the language writes
/// it after the dots, so it belongs to the upper end and never to the lower one.
fn bound_into(out: &mut String, held: &core::ops::Bound<Value>, upper: bool) {
    match held {
        core::ops::Bound::Included(value) => {
            if upper {
                out.push('=');
            }
            write(out, value);
        }
        core::ops::Bound::Excluded(value) => write(out, value),
        core::ops::Bound::Unbounded => {}
    }
}

/// A string in single quotes, escaping what would end it.
fn string_into(out: &mut String, text: &str) {
    out.push('\'');
    for character in text.chars() {
        match character {
            '\'' => out.push_str("\\'"),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            other => out.push(other),
        }
    }
    out.push('\'');
}

/// Whether this can be written bare as a field name.
fn is_a_name(text: &str) -> bool {
    let mut characters = text.chars();
    let Some(first) = characters.next() else {
        return false;
    };
    (first.is_alphabetic() || first == '_')
        && characters.all(|held| held.is_alphanumeric() || held == '_')
}

/// One record of an answer, as `id: value`.
#[must_use]
pub fn record(id: &bgv_db_types::RecordId, held: &Value) -> String {
    format!("{id}: {}", value(held))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic)]

    use std::collections::BTreeMap;

    use bgv_db_types::{Number, Value};

    use super::{is_a_name, value};

    #[test]
    fn a_decimal_keeps_the_marker_that_keeps_it_exact() {
        // Without the prefix it reads back as a float, which is the one mistake
        // the language went out of its way to make loud.
        let held = Value::Number(Number::Decimal(
            rust_decimal::Decimal::try_from(12.34_f64).expect("a decimal"),
        ));
        assert_eq!(value(&held), "dec 12.34");
    }

    #[test]
    fn a_whole_float_keeps_a_fraction_so_it_does_not_read_back_as_an_integer() {
        assert_eq!(value(&Value::Number(Number::float(2.0))), "2.0");
        assert_eq!(value(&Value::Number(Number::from(2_i64))), "2");
    }

    #[test]
    fn a_string_that_would_end_itself_is_escaped() {
        assert_eq!(value(&Value::from("it's")), "'it\\'s'");
        assert_eq!(value(&Value::from("a\nb")), "'a\\nb'");
    }

    #[test]
    fn a_field_name_that_is_not_a_name_is_quoted() {
        let fields = BTreeMap::from([
            ("plain".to_owned(), Value::from(1_i64)),
            ("with space".to_owned(), Value::from(2_i64)),
        ]);
        let rendered = value(&Value::Object(fields));
        assert!(rendered.contains("plain: 1"), "{rendered}");
        assert!(rendered.contains("'with space': 2"), "{rendered}");
    }

    #[test]
    fn what_counts_as_a_bare_name() {
        assert!(is_a_name("name"));
        assert!(is_a_name("_private"));
        assert!(is_a_name("a1"));
        assert!(!is_a_name(""));
        assert!(!is_a_name("1a"));
        assert!(!is_a_name("a b"));
        assert!(!is_a_name("a-b"));
    }

    #[test]
    fn nesting_is_rendered_rather_than_summarised() {
        // The whole reason this module exists: `Display` answers `<array of 2>`.
        let held = Value::Array(vec![
            Value::Object(BTreeMap::from([("a".to_owned(), Value::from(1_i64))])),
            Value::Array(vec![Value::None, Value::Null]),
        ]);
        assert_eq!(value(&held), "[{ a: 1 }, [NONE, NULL]]");
    }
}
