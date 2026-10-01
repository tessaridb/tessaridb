//! Counting what a search index would hold for some records (ADR-0103).

use super::Transaction;
use crate::catalog::IndexDefinition;
use crate::error::Result;
use crate::index::{analyzers_on, search_analyzer, terms_of};
use tessari_encoding::{IndexValues, decode_payload};
use tessari_types::{RecordId, Value};

/// What a search index would hold for a set of records: the figures a score
/// measures a collection by, for exactly those records.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchCounts {
    /// How many of them hold at least one term.
    pub documents: u64,
    /// How many tokens they hold in total.
    pub tokens: u64,
    /// For each asked term, in the order asked, how many of them hold it.
    pub holding: Vec<u64>,
}

impl Transaction<'_> {
    /// Count `records` as `index` would: through the function its writer
    /// analyses a record with, so the figures are the ones the index's own
    /// statistics and dictionary carry for the same records.
    ///
    /// A shard's leader answers a partial holder's scored read with these,
    /// because its own index statistics describe whatever it holds rather than
    /// one shard.
    ///
    /// # Errors
    ///
    /// Returns an error when the catalog cannot be read or a record cannot be
    /// decoded.
    pub fn search_counts(
        &mut self,
        index: &IndexDefinition,
        records: &[(RecordId, Vec<u8>)],
        terms: &[String],
    ) -> Result<SearchCounts> {
        let analyzers = analyzers_on(self, index.table)?;
        let analyzer = search_analyzer(index, &analyzers);
        let asked: Vec<IndexValues> = terms
            .iter()
            .map(|term| IndexValues::of(&[Value::from(term.as_str())]))
            .collect();
        let mut counts = SearchCounts {
            holding: vec![0; terms.len()],
            ..SearchCounts::default()
        };
        for (_, payload) in records {
            let analysed = terms_of(index, analyzer, &decode_payload(payload)?);
            // The writer's rule: a record holding no token is not in the index.
            if analysed.tokens == 0 {
                continue;
            }
            counts.documents = counts.documents.saturating_add(1);
            counts.tokens = counts.tokens.saturating_add(analysed.tokens);
            for (held, term) in counts.holding.iter_mut().zip(&asked) {
                if analysed.postings.iter().any(|(posted, _)| posted == term) {
                    *held = held.saturating_add(1);
                }
            }
        }
        Ok(counts)
    }
}
