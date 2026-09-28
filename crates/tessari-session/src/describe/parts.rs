//! Writing a definition's parts back as statements: splits, fields, indexes, assertions and literals.

use super::{Unwritable, number_literal};
use std::fmt::Write as _;
use tessari_storage::{FieldDefinition, IndexDefinition, TableDefinition};
use tessari_types::{Assertion, Operand, Value};

/// Where the table's shards begin, when it is split (G031, ADR-0080).
///
/// Each point as the literal that addresses a record, which is the spelling the
/// clause reads. Without it the script re-creates an unsplit table, and nothing
/// about the restored store is in an error state — every record it takes would
/// simply land in one shard where the original routed it to three.
pub(crate) fn write_split(script: &mut String, definition: &TableDefinition) {
    let Some(shards) = &definition.shards else {
        return;
    };
    let points: Vec<String> = shards
        .spans()
        .filter_map(|span| span.from.map(tessari_types::RecordId::to_literal))
        .collect();
    let _ = write!(script, " SPLIT AT {}", points.join(", "));
}

/// One field's declaration.
///
/// `DEFAULT` needs no rendering: the catalog stores what the author typed, so
/// the text goes back exactly as it came.
pub(crate) fn write_field(
    script: &mut String,
    table: &str,
    field: &FieldDefinition,
) -> Result<(), Unwritable> {
    let name = &field.name;
    let _ = write!(
        script,
        "DEFINE FIELD {name} ON {table} TYPE {}",
        field.kind.name()
    );
    // First among the options and never omitted. A field declared `SECRET` and
    // written back without the word restores as an ordinary field, so a schema
    // round trip through this module would un-seal every secret a vault holds —
    // silently, because the restored store is in no error state and the field
    // still carries the name, the type and the value the caller expects.
    if field.secret {
        script.push_str(" SECRET");
    }
    if field.required {
        script.push_str(" REQUIRED");
    }
    if let Some(default) = &field.default {
        let _ = write!(script, " DEFAULT {default}");
    }
    if let Some(analyzer) = &field.analyzer {
        let _ = write!(script, " ANALYZER {analyzer}");
    }
    if let Some(assert) = &field.assert {
        let written = assertion(assert)
            .ok_or_else(|| Unwritable::at(format!("the assertion on field `{name}`")))?;
        let _ = write!(script, " ASSERT {written}");
    }
    script.push_str(";\n");
    Ok(())
}

/// One index's declaration.
///
/// At most one kind word, because the grammar accepts at most one: `UNIQUE`,
/// `SEARCH`, `SPATIAL` and `VECTOR <distance>` are four index kinds and a
/// statement naming two is refused where it is written.
pub(crate) fn write_index(
    script: &mut String,
    table: &str,
    index: &IndexDefinition,
) -> Result<(), Unwritable> {
    let name = &index.name;
    let projected = index
        .fields
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    let _ = write!(script, "DEFINE INDEX {name} ON {table} FIELDS {projected}");
    match (index.unique, index.search, index.spatial, index.vector) {
        (false, false, false, None) => {}
        (true, false, false, None) => script.push_str(" UNIQUE"),
        (false, true, false, None) => script.push_str(" SEARCH"),
        (false, false, true, None) => script.push_str(" SPATIAL"),
        (false, false, false, Some(distance)) => {
            let _ = write!(script, " VECTOR {}", distance.name());
        }
        // Two kinds at once is a state the grammar refuses and the catalog
        // should not hold. Withholding the declaration is the only honest
        // answer: writing one of the two would describe a different index.
        _ => {
            return Err(Unwritable::at(format!(
                "index `{name}` carries more than one kind"
            )));
        }
    }
    script.push_str(";\n");
    Ok(())
}

/// A constraint, written the way its author wrote it.
///
/// `All` and `Any` are refused with anything other than two parts. The parser
/// lowers `a AND b` into a pair and nests for a third, so every constraint it
/// produces is binary — and a flat list of three would have to be written as
/// `(a AND b AND c)`, which re-reads as a pair whose first half is a pair. That
/// is a different constraint carrying the same words, which is exactly the class
/// of answer this module exists to withhold.
pub(crate) fn assertion(constraint: &Assertion) -> Option<String> {
    match constraint {
        Assertion::Compare { op, against } => {
            let right = match against {
                Operand::Literal(held) => literal(held)?,
                Operand::Field(route) => route.to_string(),
            };
            Some(format!("$value {} {right}", op.spelling()))
        }
        Assertion::All(parts) => pair(parts, "AND"),
        Assertion::Any(parts) => pair(parts, "OR"),
        Assertion::Not(inner) => Some(format!("(NOT {})", assertion(inner)?)),
    }
}

pub(crate) fn pair(parts: &[Assertion], word: &str) -> Option<String> {
    let [left, right] = parts else {
        return None;
    };
    Some(format!(
        "({} {word} {})",
        assertion(left)?,
        assertion(right)?
    ))
}

/// A value as the literal that produces it, or nothing.
///
/// The set is small on purpose and every member of it is a spelling the lexer
/// reads back into the value it came from. The rest are absent for one of two
/// reasons, and both are reasons to withhold rather than to approximate:
///
/// - **No literal exists.** There is no datetime, uuid, object, range,
///   geometry or regex literal in the language, so a value of those kinds
///   reached the catalog through a cast and nothing written here would parse
///   back into it.
/// - **A literal exists and this module cannot prove its spelling.** A duration
///   is written `1h30m` and displayed `90.000000000s`, which lexes as a float
///   and a stray name. A set has a literal form; writing one without a test
///   proving it re-reads would be exactly the guess this module refuses.
pub(crate) fn literal(value: &Value) -> Option<String> {
    match value {
        Value::None => Some("NONE".to_owned()),
        Value::Null => Some("NULL".to_owned()),
        Value::Bool(held) => Some(held.to_string()),
        Value::Number(number) => number_literal(number),
        Value::String(text) => quoted(text),
        Value::Array(items) => {
            let mut written = Vec::with_capacity(items.len());
            for item in items {
                written.push(literal(item)?);
            }
            Some(format!("[{}]", written.join(", ")))
        }
        Value::Bytes(_)
        | Value::Duration(_)
        | Value::Datetime(_)
        | Value::Uuid(_)
        | Value::Table(_)
        | Value::Record(_)
        | Value::Object(_)
        | Value::Range(_)
        | Value::Set(_)
        | Value::Geometry(_)
        | Value::Regex(_) => None,
    }
}

/// Text as a single-quoted literal.
///
/// Only the escapes the lexer accepts are written, and a control character it
/// has no escape for makes the whole literal **absent** rather than a string
/// carrying a raw byte. Passing one through would either fail to lex or lex as
/// something else depending on where it sat, and a script that fails to parse
/// on the one field nobody tested is the same defect as one that parses wrong.
pub(crate) fn quoted(text: &str) -> Option<String> {
    let mut out = String::with_capacity(text.len().saturating_add(2));
    out.push('\'');
    for character in text.chars() {
        match character {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            '\0' => out.push_str("\\0"),
            other if other.is_control() => return None,
            other => out.push(other),
        }
    }
    out.push('\'');
    Some(out)
}
