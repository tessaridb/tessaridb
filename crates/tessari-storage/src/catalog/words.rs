//! Synonym and stop-word sets: the query-time words a search reads (ADR-0105).
//!
//! Both are **store-wide names**, like an analyzer, because both describe a
//! language rather than a tenant's data. Both are read at query time and never
//! by the index, so replacing a set is a drop and a declaration and touches no
//! posting — which is the whole reason they are query-time.
//!
//! One system table holds both kinds, keyed by the kind and the name as text,
//! so a set needs no id, no counter and no claim on the names table: the key is
//! the name, and a second declaration under it finds the first.

use std::collections::BTreeMap;

use tessari_encoding::{decode_payload, encode_payload};
use tessari_types::{RecordId, Value};

use super::{Catalog, system};
use crate::error::{Error, Result};

/// Which of the two kinds a set is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum WordSetKind {
    /// `DEFINE SYNONYMS`: each word and the alternatives that also answer it.
    Synonyms,
    /// `DEFINE STOPWORDS`: words a query drops.
    Stopwords,
}

impl WordSetKind {
    /// The word the statement and the catalog key use.
    #[must_use]
    pub const fn word(self) -> &'static str {
        match self {
            Self::Synonyms => "synonyms",
            Self::Stopwords => "stopwords",
        }
    }
}

/// A declared set of words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WordSet {
    /// Which kind it is.
    pub kind: WordSetKind,
    /// Its name, unique among sets of its kind across the store.
    pub name: String,
    /// Each word and its alternatives. A stop-word set maps each word to
    /// nothing, so one shape carries both kinds.
    pub entries: BTreeMap<String, Vec<String>>,
}

impl WordSet {
    fn to_value(&self) -> Value {
        Value::Object(BTreeMap::from([
            ("kind".to_owned(), Value::from(self.kind.word())),
            ("name".to_owned(), Value::from(self.name.as_str())),
            (
                "entries".to_owned(),
                Value::Object(
                    self.entries
                        .iter()
                        .map(|(word, alternatives)| {
                            (
                                word.clone(),
                                Value::Array(
                                    alternatives
                                        .iter()
                                        .map(|alternative| Value::from(alternative.as_str()))
                                        .collect(),
                                ),
                            )
                        })
                        .collect(),
                ),
            ),
        ]))
    }

    fn from_value(value: &Value) -> Result<Self> {
        let malformed = |field: &'static str| Error::CatalogMalformed {
            entity: "word set",
            field,
            found: "not what a word set holds",
        };
        let Value::Object(fields) = value else {
            return Err(malformed("kind"));
        };
        let kind = match fields.get("kind") {
            Some(Value::String(word)) if word == "synonyms" => WordSetKind::Synonyms,
            Some(Value::String(word)) if word == "stopwords" => WordSetKind::Stopwords,
            _ => return Err(malformed("kind")),
        };
        let Some(Value::String(name)) = fields.get("name") else {
            return Err(malformed("name"));
        };
        let Some(Value::Object(stored)) = fields.get("entries") else {
            return Err(malformed("entries"));
        };
        let mut entries = BTreeMap::new();
        for (word, alternatives) in stored {
            let Value::Array(alternatives) = alternatives else {
                return Err(malformed("entries"));
            };
            let alternatives = alternatives
                .iter()
                .map(|alternative| match alternative {
                    Value::String(text) => Ok(text.clone()),
                    _ => Err(malformed("entries")),
                })
                .collect::<Result<Vec<_>>>()?;
            entries.insert(word.clone(), alternatives);
        }
        Ok(Self {
            kind,
            name: name.clone(),
            entries,
        })
    }
}

fn key(kind: WordSetKind, name: &str) -> RecordId {
    RecordId::Text(format!("{}:{name}", kind.word()))
}

impl Catalog<'_, '_> {
    /// Declare a set.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NameTaken`] when a set of that kind already has the name.
    pub fn create_word_set(&mut self, set: &WordSet) -> Result<()> {
        if self.word_set(set.kind, &set.name)?.is_some() {
            return Err(Error::NameTaken {
                qualified: format!("{}:{}", set.kind.word(), set.name),
            });
        }
        self.transaction.put(
            system::address(system::WORD_SETS, key(set.kind, &set.name)),
            encode_payload(&set.to_value()).into_bytes(),
        );
        Ok(())
    }

    /// A set by kind and name.
    ///
    /// # Errors
    ///
    /// A backend failure, or a stored row that is not a set.
    pub fn word_set(&self, kind: WordSetKind, name: &str) -> Result<Option<WordSet>> {
        let address = system::address(system::WORD_SETS, key(kind, name));
        match self.transaction.get(&address)? {
            Some(payload) => Ok(Some(WordSet::from_value(&decode_payload(&payload)?)?)),
            None => Ok(None),
        }
    }

    /// Remove a set; `false` when there was none.
    ///
    /// # Errors
    ///
    /// A backend failure, or a stored row that is not a set.
    pub fn drop_word_set(&mut self, kind: WordSetKind, name: &str) -> Result<bool> {
        if self.word_set(kind, name)?.is_none() {
            return Ok(false);
        }
        self.transaction
            .delete(system::address(system::WORD_SETS, key(kind, name)));
        Ok(true)
    }

    /// Every declared set, both kinds, in key order.
    ///
    /// # Errors
    ///
    /// A backend failure, or a stored row that is not a set.
    pub fn word_sets(&self) -> Result<Vec<WordSet>> {
        let mut found = Vec::new();
        for (_, payload) in self.transaction.scan_table(
            system::SYSTEM_NAMESPACE,
            system::SYSTEM_DATABASE,
            system::WORD_SETS,
        )? {
            found.push(WordSet::from_value(&decode_payload(&payload)?)?);
        }
        Ok(found)
    }
}
