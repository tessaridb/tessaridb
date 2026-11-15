//! A feed a table carries: the declared capture of its writes into a topic
//!.

use std::collections::BTreeMap;

use tessari_types::{FeedImage, Value, WriteKind};

use super::object;
use crate::error::{Error, Result};

const ENTITY: &str = "feed";
const FIELD_NAME: &str = "name";
const FIELD_TOPIC: &str = "topic";
const FIELD_ON: &str = "on";
const FIELD_IMAGE: &str = "image";
const FIELD_WHEN: &str = "when";
const FIELD_INITIAL_SCAN: &str = "initial_scan";

/// One feed, as the table's catalog entry holds it.
///
/// The topic is held by NAME rather than id, the way a view's text or an
/// event's body is kept as written: a declaration should stay legible in a
/// dump, and the name resolves against the database at capture time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedDeclaration {
    /// The name, unique on its table.
    pub name: String,
    /// The topic the messages land in; the feed owns it.
    pub topic: String,
    /// The writes it captures, sorted and without repeats.
    pub on: Vec<WriteKind>,
    /// Which image the messages carry.
    pub image: FeedImage,
    /// `WHEN`, as written: a write is captured only where it holds.
    pub when: Option<String>,
    /// Whether existing rows are scanned into the topic on the define.
    pub initial_scan: bool,
}

impl FeedDeclaration {
    /// Whether a write of this kind is captured.
    #[must_use]
    pub fn captures(&self, kind: WriteKind) -> bool {
        self.on.contains(&kind)
    }

    /// The value written inside the table's catalog entry.
    #[must_use]
    pub fn to_value(&self) -> Value {
        let mut fields = BTreeMap::from([
            (FIELD_NAME.to_owned(), Value::from(self.name.as_str())),
            (FIELD_TOPIC.to_owned(), Value::from(self.topic.as_str())),
            (
                FIELD_ON.to_owned(),
                Value::Array(
                    self.on
                        .iter()
                        .map(|kind| Value::from(kind.word()))
                        .collect(),
                ),
            ),
            (FIELD_IMAGE.to_owned(), Value::from(self.image.spelling())),
        ]);
        if let Some(when) = &self.when {
            fields.insert(FIELD_WHEN.to_owned(), Value::from(when.as_str()));
        }
        if self.initial_scan {
            fields.insert(FIELD_INITIAL_SCAN.to_owned(), Value::Bool(true));
        }
        Value::Object(fields)
    }

    /// Read a declaration back.
    ///
    /// An unrecognised write kind or image word is refused rather than
    /// skipped: a feed a later build declared with a shape this one cannot
    /// name must not be read as one that captures nothing.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CatalogMalformed`] when a field is missing or holds
    /// the wrong type.
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
        let image = match fields.get(FIELD_IMAGE) {
            Some(Value::String(word)) => {
                FeedImage::from_spelling(word).ok_or(Error::CatalogMalformed {
                    entity: ENTITY,
                    field: FIELD_IMAGE,
                    found: "an unknown image word",
                })?
            }
            other => {
                return Err(Error::CatalogMalformed {
                    entity: ENTITY,
                    field: FIELD_IMAGE,
                    found: other.map_or("none", Value::type_name),
                });
            }
        };
        Ok(Self {
            name: text(FIELD_NAME)?,
            topic: text(FIELD_TOPIC)?,
            on,
            image,
            when: match fields.get(FIELD_WHEN) {
                None => None,
                Some(_) => Some(text(FIELD_WHEN)?),
            },
            initial_scan: matches!(fields.get(FIELD_INITIAL_SCAN), Some(Value::Bool(true))),
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::FeedDeclaration;
    use tessari_types::{FeedImage, WriteKind};

    #[test]
    fn a_declaration_reads_back_as_written() {
        for when in [None, Some("$after.total > 100".to_owned())] {
            for initial_scan in [false, true] {
                let declared = FeedDeclaration {
                    name: "orders_feed".to_owned(),
                    topic: "order_events".to_owned(),
                    on: vec![WriteKind::Create, WriteKind::Delete],
                    image: FeedImage::Keys,
                    when: when.clone(),
                    initial_scan,
                };
                assert_eq!(
                    FeedDeclaration::from_value(&declared.to_value()).unwrap(),
                    declared
                );
            }
        }
    }
}
