//! A search member's postings read with their per-field counts (Q-870).

use std::collections::BTreeMap;

use tessari_encoding::{IndexAddress, IndexValues, KeyKind, Posting, PostingKey, StoreKey};
use tessari_kv::{KeyRange, ScanDirection, ScanRequest};
use tessari_types::{RecordId, Value};

use super::Transaction;
use crate::catalog::IndexDefinition;
use crate::error::Result;

/// For one record: each term it holds among those asked, with the term's
/// frequency and the field's length per member field.
pub type FieldedPostings = BTreeMap<RecordId, BTreeMap<String, Vec<(u32, u32)>>>;

impl Transaction<'_> {
    /// Every posting of `terms` in a search member, keyed by record, with the
    /// per-field counts the posting carries — or `None` when any posting reached
    /// carries none, because the member was written before they were kept and
    /// only its records' text can say which field a word is in.
    ///
    /// The postings of the reader's snapshot as the index holds them now: the
    /// caller asks only when the indexes are current and this transaction has
    /// not written the member's table, which is when the postings and the
    /// records describe one state.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails or a posting cannot be decoded.
    pub fn member_postings(
        &self,
        index: &IndexDefinition,
        terms: &[String],
    ) -> Result<Option<FieldedPostings>> {
        let address = IndexAddress::new(index.namespace, index.database, index.table, index.id);
        let mut found: FieldedPostings = BTreeMap::new();
        let mut fielded = true;
        for term in terms {
            let encoded = IndexValues::of(&[Value::from(term.as_str())]);
            let prefix = PostingKey::term_prefix(&address, &encoded);
            self.walk_range(
                PostingKey::keyspace(),
                &KeyRange::prefix(&prefix),
                |key, value| {
                    let fields = Posting::located(value.as_slice())?.fields;
                    if fields.is_empty() {
                        fielded = false;
                        return Ok(());
                    }
                    let id = PostingKey::decode(key.as_slice())?.id;
                    found.entry(id).or_default().insert(term.clone(), fields);
                    Ok(())
                },
            )?;
            if !fielded {
                return Ok(None);
            }
        }
        Ok(Some(found))
    }

    /// Whether a search member's postings keep per-field counts — read off its
    /// first posting, since one build writes them all one way. A member with
    /// no postings answers `true`: there is nothing it could fail to say.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails or the posting cannot be read.
    pub fn member_fielded(&self, index: &IndexDefinition) -> Result<bool> {
        let address = IndexAddress::new(index.namespace, index.database, index.table, index.id);
        let request = ScanRequest {
            keyspace: PostingKey::keyspace(),
            range: KeyRange::prefix(&address.prefix(KeyKind::Posting)),
            direction: ScanDirection::Forward,
            limit: Some(1),
        };
        match self.store.backend().scan(&request)?.first() {
            Some((_, value)) => Ok(!Posting::located(value.as_slice())?.fields.is_empty()),
            None => Ok(true),
        }
    }
}
