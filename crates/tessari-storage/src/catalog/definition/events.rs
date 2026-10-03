//! An event a table carries: what runs after each write of one of its records
//! (ADR-0110).

use std::collections::BTreeMap;

use tessari_types::{Value, WriteKind};

use super::object;
use crate::error::{Error, Result};

const ENTITY: &str = "event";
const FIELD_NAME: &str = "name";
const FIELD_ON: &str = "on";
const FIELD_WHEN: &str = "when";
const FIELD_BODY: &str = "body";

/// One event, as the table's catalog entry holds it.
///
/// The condition and the body are **source text** — the reason a view keeps its
/// read as text: a stored syntax tree would need a version every time the
/// grammar grew, and an event written before a clause existed would decode into
/// a body that had silently lost it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventDeclaration {
    /// The name, unique on its table.
    pub name: String,
    /// The writes that run it, sorted and without repeats.
    pub on: Vec<WriteKind>,
    /// `WHEN`, as written.
    pub when: Option<String>,
    /// The statements, as written.
    pub body: String,
}

impl EventDeclaration {
    /// Whether a write of this kind runs it.
    #[must_use]
    pub fn runs_on(&self, kind: WriteKind) -> bool {
        self.on.contains(&kind)
    }

    /// The value written inside the table's catalog entry.
    #[must_use]
    pub fn to_value(&self) -> Value {
        let mut fields = BTreeMap::from([
            (FIELD_NAME.to_owned(), Value::from(self.name.as_str())),
            (
                FIELD_ON.to_owned(),
                Value::Array(
                    self.on
                        .iter()
                        .map(|kind| Value::from(kind.word()))
                        .collect(),
                ),
            ),
            (FIELD_BODY.to_owned(), Value::from(self.body.as_str())),
        ]);
        if let Some(when) = &self.when {
            fields.insert(FIELD_WHEN.to_owned(), Value::from(when.as_str()));
        }
        Value::Object(fields)
    }

    /// Read a declaration back.
    ///
    /// An unrecognised write kind is refused rather than skipped: an event a
    /// later build declared for a write this one cannot name must not be read as
    /// one that never runs for it.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CatalogMalformed`] when a field is missing or holds the
    /// wrong type.
    pub fn from_value(value: &Value) -> Result<Self> {
        let fields = object(value, ENTITY)?;
        let text = |field: &'static str| match fields.get(field) {
            Some(Value::String(held)) => Ok(held.clone()),
            other => Err(Error::CatalogMalformed {
                entity: ENTITY,
                field,
                found: other.map_or("none", Value::type_name),
            }),
        };
        let Some(Value::Array(kinds)) = fields.get(FIELD_ON) else {
            return Err(Error::CatalogMalformed {
                entity: ENTITY,
                field: FIELD_ON,
                found: fields.get(FIELD_ON).map_or("none", Value::type_name),
            });
        };
        let on = kinds
            .iter()
            .map(|kind| match kind {
                Value::String(word) => WriteKind::from_word(word),
                _ => None,
            })
            .collect::<Option<Vec<_>>>()
            .ok_or(Error::CatalogMalformed {
                entity: ENTITY,
                field: FIELD_ON,
                found: "an unknown write kind",
            })?;
        Ok(Self {
            name: text(FIELD_NAME)?,
            on,
            when: match fields.get(FIELD_WHEN) {
                None => None,
                Some(_) => Some(text(FIELD_WHEN)?),
            },
            body: text(FIELD_BODY)?,
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::EventDeclaration;
    use tessari_types::WriteKind;

    #[test]
    fn a_declaration_reads_back_as_written() {
        for when in [None, Some("$after.v > 1".to_owned())] {
            let declared = EventDeclaration {
                name: "audit".to_owned(),
                on: vec![WriteKind::Create, WriteKind::Delete],
                when,
                body: "CREATE log = { v: $after.v }".to_owned(),
            };
            assert_eq!(
                EventDeclaration::from_value(&declared.to_value()).unwrap(),
                declared
            );
        }
    }
}
