//! Records as an aligned table, where they are the shape for one.
//!
//! # Why this is a rule rather than a switch
//!
//! Anybody arriving from a relational shell expects columns, and columns are
//! genuinely better for the thing they are good at: twenty rows of the same four
//! fields, scanned down a column for the odd one out. A document renderer makes
//! that job harder than it needs to be.
//!
//! But this store holds documents, and a document is not always a row. A record
//! with a nested object in it has no honest column: the choice is to truncate it,
//! to print it inline and destroy the alignment that was the whole point, or to
//! show a placeholder and hide the data somebody asked for. All three are worse
//! than printing the document.
//!
//! So the shape decides. Records that share **one flat set of fields** print as a
//! table; anything else prints as documents. `.mode` overrides in either
//! direction, because an override is cheap and being wrong about somebody's
//! terminal is not.
//!
//! # The cells are still TessariQL
//!
//! A string keeps its quotes, where a relational shell would print it bare. In a
//! schemaless store `'12'` and `12` are different answers, and a table that drew
//! them the same would be hiding the one thing a person is at a prompt to find
//! out.
//!
//! # Why the shapes must match exactly
//!
//! A union of the field sets, with blanks where a record has none, would table
//! more results. It would also make *absent* and *empty* look identical in a
//! store where they are different answers — and it would do it silently, in the
//! rendering, which is the last place a distinction should be lost. Records that
//! disagree about their fields are exactly the case a document view is for.

use std::collections::BTreeMap;

use tessari_types::Value;

use crate::render::{self, Names};

/// How answers are drawn.
///
/// Which of these a session starts in is decided by where the output is going,
/// not by a default here: a prompt starts in [`Shape::Auto`] and a script starts
/// in [`Shape::Document`], because a table cannot be pasted back into a
/// statement and this program prints TessariQL so that it can be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    /// A table when the records are the shape for one, documents otherwise.
    Auto,
    /// A table when one is possible, and documents when it is not — an override
    /// that cannot invent columns for records that have none.
    Table,
    /// Documents always.
    Document,
}

impl Shape {
    /// The mode named by `word`, or `None` for a word that names none.
    pub fn named(word: &str) -> Option<Self> {
        match word {
            "auto" => Some(Self::Auto),
            "table" => Some(Self::Table),
            "document" => Some(Self::Document),
            _ => None,
        }
    }

    /// What this mode is called, which is what `.mode` prints back.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Table => "table",
            Self::Document => "document",
        }
    }

    /// These records as a table, or `None` to print them as documents.
    #[must_use]
    pub fn drawn(self, records: &[(String, Value)], names: &Names) -> Option<String> {
        match self {
            Self::Document => None,
            Self::Auto | Self::Table => draw(records, names),
        }
    }
}

/// Whether a value has a single-line rendering that belongs in a cell.
///
/// The four that do not are the four that hold other values. Everything else —
/// including bytes, which can be long — renders on one line, and a long line is
/// a wide column rather than a broken table.
const fn flat(held: &Value) -> bool {
    !matches!(
        held,
        Value::Array(_) | Value::Object(_) | Value::Set(_) | Value::Range(_)
    )
}

/// The document behind a record, if the record is one flat document.
fn document(held: &Value) -> Option<&BTreeMap<String, Value>> {
    let Value::Object(fields) = held else {
        return None;
    };
    fields.values().all(flat).then_some(fields)
}

/// The fields every record shares, in the order a table should show them.
///
/// `BTreeMap` is what holds a document, so the order is the store's own and is
/// the same for every record by construction — there is no question of one row
/// putting its columns in a different order from another.
fn shape(records: &[(String, Value)]) -> Option<Vec<String>> {
    let mut agreed: Option<Vec<String>> = None;
    for (_, held) in records {
        let here: Vec<String> = document(held)?.keys().cloned().collect();
        match &agreed {
            None => agreed = Some(here),
            Some(known) if *known == here => {}
            Some(_) => return None,
        }
    }
    agreed.filter(|fields| !fields.is_empty())
}

/// The table, or `None` where these records are not one.
fn draw(records: &[(String, Value)], names: &Names) -> Option<String> {
    let fields = shape(records)?;

    let mut header = Vec::with_capacity(fields.len().saturating_add(1));
    header.push("id".to_owned());
    header.extend(fields.iter().cloned());

    // A column of numbers reads right-aligned and a column of anything else
    // reads left-aligned, which is the one piece of typesetting a table like
    // this actually needs: it is what lets a column of amounts be scanned for
    // the large one without reading any of them.
    let mut rightwards = Vec::with_capacity(header.len());
    rightwards.push(false);
    for field in &fields {
        rightwards.push(records.iter().all(|(_, held)| {
            matches!(
                document(held).and_then(|held| held.get(field)),
                Some(Value::Number(_))
            )
        }));
    }

    let mut rows = Vec::with_capacity(records.len());
    for (id, held) in records {
        let held = document(held)?;
        let mut row = Vec::with_capacity(header.len());
        row.push(id.clone());
        for field in &fields {
            row.push(
                held.get(field)
                    .map_or_else(String::new, |value| render::value(value, names)),
            );
        }
        rows.push(row);
    }

    let mut widths: Vec<usize> = header.iter().map(|cell| cell.chars().count()).collect();
    for row in &rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }

    let mut out = String::new();
    line(&mut out, &header, &widths, &vec![false; widths.len()]);
    let rule: Vec<String> = widths
        .iter()
        .map(|width| "-".repeat(width.saturating_add(2)))
        .collect();
    out.push_str(&rule.join("+"));
    out.push('\n');
    for row in &rows {
        line(&mut out, row, &widths, &rightwards);
    }
    Some(out)
}

/// One row, padded to the column widths and trimmed of the trailing space.
fn line(out: &mut String, cells: &[String], widths: &[usize], rightwards: &[bool]) {
    let drawn: Vec<String> = cells
        .iter()
        .enumerate()
        .map(|(at, cell)| {
            let width = widths.get(at).copied().unwrap_or_default();
            let padding = " ".repeat(width.saturating_sub(cell.chars().count()));
            if rightwards.get(at).copied().unwrap_or_default() {
                format!(" {padding}{cell} ")
            } else {
                format!(" {cell}{padding} ")
            }
        })
        .collect();
    out.push_str(drawn.join("|").trim_end());
    out.push('\n');
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic)]

    use std::collections::BTreeMap;

    use tessari_types::{Number, Value};

    use super::{Shape, draw};
    use crate::render::Names;

    fn record(id: &str, fields: &[(&str, Value)]) -> (String, Value) {
        let held: BTreeMap<String, Value> = fields
            .iter()
            .map(|(name, value)| ((*name).to_owned(), value.clone()))
            .collect();
        (id.to_owned(), Value::Object(held))
    }

    fn text(what: &str) -> Value {
        Value::String(what.to_owned())
    }

    fn count(what: i64) -> Value {
        Value::Number(Number::Integer(what))
    }

    #[test]
    fn records_of_one_shape_become_a_table_with_the_numbers_to_the_right() {
        let records = vec![
            record("users:1", &[("name", text("ada")), ("visits", count(12))]),
            record("users:2", &[("name", text("grace")), ("visits", count(3))]),
        ];
        let drawn = draw(&records, &Names::new()).expect("a table");
        assert_eq!(
            drawn,
            " id      | name    | visits\n\
             ---------+---------+--------\n\
             \x20users:1 | 'ada'   |     12\n\
             \x20users:2 | 'grace' |      3\n"
        );
    }

    #[test]
    fn a_nested_field_falls_back_to_documents() {
        // The failure this prevents is not a crash: it is a table with a column
        // that had to lie about what is in it.
        let records = vec![record(
            "users:1",
            &[
                ("name", text("ada")),
                ("tags", Value::Array(vec![text("a"), text("b")])),
            ],
        )];
        assert!(draw(&records, &Names::new()).is_none());
    }

    #[test]
    fn records_that_disagree_about_their_fields_fall_back_to_documents() {
        // A union with blanks would table these, and would make "no such field"
        // and "an empty value" print identically.
        let records = vec![
            record("users:1", &[("name", text("ada"))]),
            record("users:2", &[("name", text("grace")), ("visits", count(3))]),
        ];
        assert!(draw(&records, &Names::new()).is_none());
    }

    #[test]
    fn the_document_mode_refuses_a_table_the_others_would_draw() {
        let records = vec![record("users:1", &[("name", text("ada"))])];
        assert!(Shape::Auto.drawn(&records, &Names::new()).is_some());
        assert!(Shape::Table.drawn(&records, &Names::new()).is_some());
        assert!(Shape::Document.drawn(&records, &Names::new()).is_none());
    }

    #[test]
    fn a_record_that_is_not_a_document_at_all_has_no_table() {
        // A space holds a single value per record, so this is a real answer
        // rather than a contrived one.
        let records = vec![("sessions:1".to_owned(), text("open"))];
        assert!(draw(&records, &Names::new()).is_none());
    }
}
