//! `FROM SEARCH s COMPLETE 'lov'`: the words the search holds that begin with
//! what was typed, ranked by how many records hold them (ADR-0105 D7).
//!
//! The dictionary walk of `MATCHES PREFIX`, over every member the read
//! reaches, with the same three-character floor. The rank is the summed
//! document frequency, ties to the word, so the same store always offers the
//! same words in the same order. With a stemming analyzer the words offered are
//! stems; a type-ahead box wants a search declared over an unstemmed analyzer.

use std::collections::BTreeMap;

use tessari_constants::{SEARCH_PREFIX_MINIMUM, SEARCH_PREFIX_SCORE_EXAMINATION_CAP};
use tessari_ql::Span;
use tessari_storage::Transaction;
use tessari_types::{Number, RecordId, Value};

use super::Resolved;
use crate::error::{Error, Result};
use crate::session::Session;

impl Session<'_> {
    /// The completions of `text`, best first, each a row `{ term, documents }`
    /// whose identity is the term.
    pub(crate) fn complete(
        &self,
        transaction: &mut Transaction<'_>,
        resolved: &Resolved,
        text: &str,
        span: Span,
    ) -> Result<Vec<(RecordId, Value)>> {
        let Some(typed) = resolved.analyzer.prefixes(text).pop() else {
            return Ok(Vec::new());
        };
        if let Some(first) = typed.first()
            && first.chars().count() < SEARCH_PREFIX_MINIMUM
        {
            return Err(Error::PrefixTooShort {
                prefix: first.clone(),
                minimum: SEARCH_PREFIX_MINIMUM,
                span,
            });
        }
        let mut held: BTreeMap<String, u64> = BTreeMap::new();
        for member in &resolved.members {
            for spelling in &typed {
                let found = transaction.terms_with_prefix(
                    &member.index,
                    spelling,
                    SEARCH_PREFIX_SCORE_EXAMINATION_CAP,
                )?;
                for term in found.terms {
                    let documents = transaction.document_frequency(&member.index, &term)?;
                    let entry = held.entry(term).or_default();
                    *entry = entry.saturating_add(documents);
                }
            }
        }
        let mut ranked: Vec<(String, u64)> = held.into_iter().collect();
        ranked.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
        Ok(ranked
            .into_iter()
            .map(|(term, documents)| {
                let row = Value::Object(BTreeMap::from([
                    ("term".to_owned(), Value::from(term.as_str())),
                    (
                        "documents".to_owned(),
                        Value::Number(Number::Integer(
                            i64::try_from(documents).unwrap_or(i64::MAX),
                        )),
                    ),
                ]));
                (RecordId::Text(term), row)
            })
            .collect())
    }
}
