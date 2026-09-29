//! Keeping index entries in step with the records they describe.
//!
//! # Entries are derived, never logged
//!
//! A log record carries record mutations and nothing else. Index entries are
//! computed from those mutations when the record is applied, which is what makes
//! a replica's indexes match the leader's without anything being sent: apply is
//! a pure function of the log entry and the catalog, and the catalog is itself
//! in the log, so every replica computes the same entries at the same sequence.
//!
//! Carrying the entries in the log would work too, and would cost log size and
//! a second source of truth that a rebuild could disagree with.
//!
//! # Two rules that follow from the value system
//!
//! **A record missing an indexed field is not indexed at all.** `none` means the
//! field is not there, so there is no value to place. Indexing it as `none`
//! instead would make every record lacking the field collide in a unique index,
//! which is a constraint nobody asked for.
//!
//! **A record whose field holds `null` *is* indexed**, under `null`. It is a
//! value, and two records holding it collide in a unique index the same way two
//! records holding `7` do. That is the point of keeping absent and null apart.
//!
//! # An index is a candidate set, not an answer
//!
//! Index entries hold the **current** state: they carry no version, and an
//! update removes the entry for the value it replaced. A transaction reading at
//! an older snapshot therefore cannot trust an index scan on its own — it must
//! resolve each candidate record at its own snapshot, and it may miss a record
//! whose indexed value has since changed. Reading at the latest committed state
//! is exact.
//!
//! That is a real limitation and it is written here rather than discovered by
//! the first query that returns the wrong rows. Removing it means versioning the
//! entries and reclaiming old ones in the background, which is a larger piece of
//! work than this one and is not started.
//!
//! # An index is built in the commit that defines it
//!
//! Maintenance sees mutations, and rows written before an index existed are not
//! mutations in the record that defines it. Left there, an index declared on a
//! populated table would know nothing about those rows — and since a filter is
//! served by an index when one exists and by a scan when one does not, the same
//! query would answer with *fewer* records and raise nothing.
//!
//! So a definition is treated as what it is. A catalog entry is an ordinary
//! record in the system tenancy (ADR-0009), so `DEFINE INDEX` arrives here as a
//! mutation like any other, and [`build`] projects the table's rows under the
//! new index into the **same batch**. The definition and its entries land
//! together or not at all, and a replica computes the same entries from the same
//! record — no new log shape, and nothing for a caller to remember.
//!
//! The rows it indexes are the committed ones *overlaid with this record's own
//! mutations*, because the per-mutation path above cannot cover them: it reads
//! the catalog below this commit, where the index does not exist yet.
//!
//! **What it costs.** Defining an index reads the whole table inside the commit.
//! On a table large enough that the pass outlasts the gap between writes, the
//! commit's compare-and-set on the applied position loses repeatedly and the
//! statement fails with [`Error::CommitContention`] — it does not half-build.
//! The answer is a resumable watermark, whose key kind is reserved (`0x37`) and
//! whose work is not started.
//!
//! One consequence worth naming: a `UNIQUE` index over rows that **already**
//! violate it is now refused at the moment it is defined, because the claims
//! collide while the batch is still being built. A unique constraint can no
//! longer be declared and unenforced.

mod build;
mod entries;
mod projecting;
mod settling;
use std::collections::{BTreeMap, BTreeSet};

use tessari_encoding::{
    IndexAddress, IndexValues, LogRecord, Mutation, Posting, PostingKey, RecordValue, StoreKey,
    StoreValue, decode_payload, encode_payload,
};
use tessari_kv::WriteBatch;
use tessari_types::{Analyzer, DatabaseId, NamespaceId, TableId, Value};

use crate::catalog::{Catalog, IndexDefinition, defined_index};
use crate::error::Result;
use crate::graph;
use crate::store::Store;
use crate::transaction::RecordAddress;
pub(crate) use build::build;
pub(crate) use entries::{displace, insert, place, place_cells, remove};
pub(crate) use projecting::{
    analyzers_on, covering_of, project, projected_vector, search_analyzer, terms_of,
};
pub(crate) use settling::{Delta, settle};

/// What one log record accumulates while its index writes are built.
///
/// Every field is a fact a single mutation cannot see on its own: the unique
/// values already claimed *within this batch*, how each search index's
/// collection statistics have moved so far, and which indexes this record
/// builds outright. They travel together because they have the same lifetime
/// and the same reason to exist — the batch is the unit of atomicity, so it is
/// also the unit these are true of.
#[derive(Debug, Default)]
pub(crate) struct Pending {
    /// Unique index keys this batch has already written.
    ///
    /// Two records in ONE batch claiming one unique value would each find the
    /// key absent and each write it, and the second would silently overwrite the
    /// first. A precondition cannot catch that — both are satisfied.
    claimed: BTreeSet<Vec<u8>>,
    /// How each search index's statistics move, written once at the end.
    moved: BTreeMap<IndexAddress, Delta>,
    /// How each term's document count moves, per search index.
    ///
    /// Accumulated for the reason [`Self::moved`] is, and it matters more here:
    /// a batch that rewrites a thousand records touching one common word would
    /// otherwise read and write that word's entry a thousand times. Folded once
    /// per term at the end, it is one read and one write.
    ///
    /// Signed, because a record leaving the index takes its terms with it — and
    /// a term reaching zero has its entry **deleted** rather than written as
    /// zero. A dictionary holding words no record contains would answer a prefix
    /// walk with terms whose posting lists are empty, which is the one thing the
    /// dictionary exists to stop.
    terms: BTreeMap<IndexAddress, BTreeMap<IndexValues, Moved>>,
    /// Indexes [`build`] wrote whole in this record.
    ///
    /// Their statistics are a **total**, not a movement: the build counted every
    /// row the index has, so adding that to the stored figure would count each
    /// document a second time. `settle` reads this to know which of the two it
    /// is holding.
    built: BTreeSet<IndexAddress>,
}

/// How one term's dictionary entry moves in this batch.
///
/// The count and the pruning bound travel together because they are two answers
/// about one term derived from one pass over the same postings. Kept in separate
/// maps they would be updated in separate loops, and the failure that follows is
/// the one this store's rules single out: a bound that does not describe the
/// postings the count describes is not a slow bound, it is an unsound one, and it
/// removes records from an answer without anything being in an error state.
#[derive(Debug, Clone, Copy, Default)]
struct Moved {
    /// How the document count moves — signed, because a record leaving the index
    /// takes its terms with it.
    delta: i64,
    /// The most occurrences any posting **arriving** in this batch records.
    ///
    /// Zero when nothing arrived, which is the identity for a maximum.
    frequency: u32,
    /// The fewest tokens held by any record **arriving** in this batch.
    ///
    /// `None` rather than a sentinel: the identity for a minimum is not a value
    /// this type can hold, and `u32::MAX` standing in for one would be a real
    /// length as far as every comparison below is concerned.
    length: Option<u32>,
}

impl Moved {
    /// Record a posting arriving.
    fn arrived(&mut self, frequency: u32, length: u32) {
        self.delta = self.delta.saturating_add(1);
        self.frequency = self.frequency.max(frequency);
        self.length = Some(self.length.map_or(length, |held| held.min(length)));
    }

    /// Record a posting leaving.
    ///
    /// The extremes are deliberately untouched. An extreme cannot move inward
    /// without knowing the second one, and reading the term's whole posting range
    /// to find it would put an O(df) scan on every delete. So the bound stays
    /// sound and grows loose, which is the compromise ADR-0050 states and the
    /// direction it insists on: loose costs pruning efficiency, wrong costs
    /// records.
    fn left(&mut self) {
        self.delta = self.delta.saturating_sub(1);
    }
}

/// Add the index writes a log record implies to `batch`.
///
/// Reads the catalog and the records' current values as of the committed state,
/// which is the state this record is about to be applied on top of.
pub(crate) fn maintain(store: &Store, record: &LogRecord, batch: WriteBatch) -> Result<WriteBatch> {
    maintain_from(store, store.begin()?, record, batch)
}

/// [`maintain`], reading the records' current values through `view`.
///
/// The series removal pass hands in a view that sees below its table's floor:
/// through an ordinary one a record past the floor has no current value, so
/// removing it would take none of its index entries with it.
pub(crate) fn maintain_from(
    store: &Store,
    mut view: crate::transaction::Transaction<'_>,
    record: &LogRecord,
    mut batch: WriteBatch,
) -> Result<WriteBatch> {
    // Keyed by the WHOLE tenancy and not by the table alone, because a `TableId`
    // is not a key on its own. Ids are handed out store-wide from
    // `system::FIRST_ID`, and the system catalog reserves the first eighteen at
    // namespace 0, database 0 — `DATABASES` is 2, `TABLES` is 3. So the first
    // eighteen tables anybody declares carry a number a system table also
    // carries, and asking for "the indexes on table 2" answered with a user's
    // index when the mutation was a `DEFINE DATABASE`.
    //
    // The entries then landed in the USER's keyspace, because `apply_one` builds
    // its address from the definition — correctly. That is what made the two
    // halves add up to a wrong answer: chosen with a partial key, applied with
    // the whole one. Two databases in different namespaces could not share a
    // name, and an application row could not hold a value that was also some
    // database's name, both refused by an index neither statement mentioned.
    let mut by_table: BTreeMap<(NamespaceId, DatabaseId, TableId), Vec<IndexDefinition>> =
        BTreeMap::new();
    // The analyzer a search index uses is the **field's** declaration, not the
    // index's, so it is read from the schema here — once per table rather than
    // once per record. That is what makes a scan and an index answer the same
    // question; see `tessari_types::Analyzer`.
    let mut analyzers: BTreeMap<TableId, BTreeMap<String, Analyzer>> = BTreeMap::new();
    let mut pending = Pending::default();

    for mutation in record.mutations() {
        let at = (mutation.namespace, mutation.database, mutation.table);
        let definitions = match by_table.get(&at) {
            Some(found) => found.clone(),
            None => {
                let found: Vec<IndexDefinition> = Catalog::new(&mut view)
                    .indexes_on(mutation.table)?
                    .into_iter()
                    .filter(|definition| {
                        definition.namespace == mutation.namespace
                            && definition.database == mutation.database
                    })
                    .collect();
                by_table.insert(at, found.clone());
                found
            }
        };
        if definitions.is_empty() {
            continue;
        }
        let declared = match analyzers.get(&mutation.table) {
            Some(found) => found.clone(),
            None => {
                let found = analyzers_on(&mut view, mutation.table)?;
                analyzers.insert(mutation.table, found.clone());
                found
            }
        };

        let address = RecordAddress::new(
            mutation.namespace,
            mutation.database,
            mutation.table,
            mutation.id.clone(),
        );
        let previous = view.get_held(&address)?;
        let unchanged = Unchanged::of(previous.as_deref(), mutation)?;

        for definition in &definitions {
            if unchanged.reads_the_same(definition) {
                continue;
            }
            batch = apply_one(
                store,
                batch,
                definition,
                mutation,
                previous.as_deref(),
                &declared,
                &mut pending,
            )?;
        }
    }

    // Second pass, and it has to be second: an index defined by this record is
    // invisible to the catalog read above, which sees the state this record is
    // about to be applied on top of.
    for mutation in record.mutations() {
        if let Some(definition) = defined_index(mutation)? {
            batch = build(store, batch, &mut view, record, &definition, &mut pending)?;
        }
    }
    settle(store, batch, &pending)
}

fn apply_one(
    store: &Store,
    mut batch: WriteBatch,
    definition: &IndexDefinition,
    mutation: &Mutation,
    previous: Option<&[u8]>,
    analyzers: &BTreeMap<String, Analyzer>,
    pending: &mut Pending,
) -> Result<WriteBatch> {
    let address = IndexAddress::new(
        definition.namespace,
        definition.database,
        definition.table,
        definition.id,
    );

    if let Some(distance) = definition.vector {
        // The graph is read from committed state and edited, then the nodes the
        // edit touched are written. Reading the whole graph per mutation is the
        // cost this shape pays, and it is stated in `graph.rs` rather than
        // discovered: an index over more vectors than fit in memory wants a
        // paging walk, which is not this.
        let mut graph = graph::Graph::read(store, &address, distance)?;
        let previous_vector = previous
            .map(decode_payload)
            .transpose()?
            .and_then(|held| projected_vector(definition, &held));
        if previous_vector.is_some() {
            graph.remove(&mutation.id);
            batch = graph::erase(batch, &address, &mutation.id);
        }
        if let RecordValue::Present(payload) = mutation.value.value()
            && let Some(held) = projected_vector(definition, &decode_payload(payload)?)
        {
            let touched = graph.insert(&mutation.id, held);
            batch = graph::write(batch, &address, &touched);
        }
        return Ok(batch);
    }

    if definition.spatial {
        // Both sides enumerate with the same function, so a record that kept its
        // geometry writes back exactly the keys it already had and a record that
        // changed it leaves none behind. Reasoning about *what moved* instead is
        // where an orphan cell would come from — and an orphan here is a record
        // answering a box it is no longer inside, which no reader would question
        // because the answer is geographically plausible.
        if let Some(bytes) = previous {
            batch = displace(
                batch,
                &address,
                &mutation.id,
                &decode_payload(bytes)?,
                definition,
            );
        }
        if let RecordValue::Present(payload) = mutation.value.value() {
            batch = place(
                batch,
                &address,
                &mutation.id,
                &decode_payload(payload)?,
                definition,
            );
        }
        return Ok(batch);
    }

    if definition.search {
        let analyzer = search_analyzer(definition, analyzers);
        let counted = pending.moved.entry(address).or_default();
        let dictionary = pending.terms.entry(address).or_default();
        // The old side first, and both sides of the same change: a record whose
        // text changed leaves the index at its former length and re-enters at
        // its new one, so a statistic that only counted arrivals would drift
        // upward by exactly the amount nobody ever looks at.
        //
        // The dictionary moves on the same two sides and by the same reasoning.
        // A word the record kept is decremented and incremented, netting zero,
        // so a rewrite that changed one sentence does not disturb the frequency
        // of every other word in the document.
        if let Some(bytes) = previous {
            let analysed = terms_of(definition, analyzer, &decode_payload(bytes)?);
            counted.removed(analysed.tokens);
            for (term, _) in analysed.postings {
                dictionary.entry(term.clone()).or_default().left();
                batch = batch.delete(
                    PostingKey::keyspace(),
                    PostingKey::new(address, term, mutation.id.clone()).encode(),
                );
            }
        }
        if let RecordValue::Present(payload) = mutation.value.value() {
            let analysed = terms_of(definition, analyzer, &decode_payload(payload)?);
            counted.added(analysed.tokens);
            let length = analysed.length();
            for (term, frequency) in analysed.postings {
                dictionary
                    .entry(term.clone())
                    .or_default()
                    .arrived(frequency, length);
                batch = batch.put(
                    PostingKey::keyspace(),
                    PostingKey::new(address, term, mutation.id.clone()).encode(),
                    Posting::Counted { frequency, length }.encode(),
                );
            }
        }
        return Ok(batch);
    }

    // The old entries go first: a record whose indexed value changed must not
    // leave the entry that pointed at its former value behind, and an entry
    // nothing will ever reconcile is the failure mode secondary indexes are
    // known for.
    //
    // With a multi-valued route there is one entry per element, so removing an
    // element has to remove **exactly its own** entry and no other. That holds
    // because both sides enumerate with the same function: the old record's
    // entries are deleted and the new record's are written, and the elements the
    // record kept are written back under the keys they already had. A remove that
    // reasoned about *what changed* instead is where the orphan would come from.
    if let Some(bytes) = previous {
        for values in project(definition, &decode_payload(bytes)?) {
            batch = remove(batch, definition, &address, &values, &mutation.id);
        }
    }

    if let RecordValue::Present(payload) = mutation.value.value() {
        for values in project(definition, &decode_payload(payload)?) {
            batch = insert(
                store,
                batch,
                definition,
                &address,
                &values,
                &mutation.id,
                &mut pending.claimed,
            )?;
        }
    }
    Ok(batch)
}

/// A record's value before and after one mutation, decoded once for every index.
///
/// Empty unless the mutation rewrites a record that existed: an insert and a
/// delete change every index that reads the record at all.
struct Unchanged {
    sides: Option<(Value, Value)>,
}

impl Unchanged {
    fn of(previous: Option<&[u8]>, mutation: &Mutation) -> Result<Self> {
        let sides = match (previous, mutation.value.value()) {
            (Some(before), RecordValue::Present(after)) => {
                Some((decode_payload(before)?, decode_payload(after)?))
            }
            _ => None,
        };
        Ok(Self { sides })
    }

    /// Whether every field `definition` reads holds exactly what it held, so its
    /// entries — and a full-text index's statistics — are what they already are.
    ///
    /// Compared as encoded bytes, not as values: `1` and `1.0` are equal values
    /// written differently, and an entry built from one is not the entry of the
    /// other. A vector index is never skipped, because re-inserting a node moves
    /// the graph's edges and an approximate read over it can answer differently.
    fn reads_the_same(&self, definition: &IndexDefinition) -> bool {
        let Some((before, after)) = &self.sides else {
            return false;
        };
        definition.vector.is_none()
            && definition.fields.iter().all(|path| {
                let (was, now) = (path.reach(before), path.reach(after));
                was.len() == now.len()
                    && was
                        .iter()
                        .zip(&now)
                        .all(|(was, now)| encode_payload(was) == encode_payload(now))
            })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic, clippy::unwrap_used)]

    use tessari_types::{Analyzer, DatabaseId, Filter, IndexId, NamespaceId, Path, TableId, Value};

    use super::terms_of;
    use crate::catalog::IndexDefinition;

    fn definition() -> IndexDefinition {
        IndexDefinition {
            id: IndexId::new(1),
            namespace: NamespaceId::new(1),
            database: DatabaseId::new(1),
            table: TableId::new(1),
            name: "by_body".to_owned(),
            fields: vec![Path::field("body")],
            search: true,
            unique: false,
            vector: None,
            spatial: false,
        }
    }

    fn record(text: &str) -> Value {
        Value::Object(
            [("body".to_owned(), Value::from(text))]
                .into_iter()
                .collect(),
        )
    }

    /// The frequency of each term, keyed by the term as written.
    fn analysed(text: &str) -> (Vec<u32>, u64) {
        let analyzer = Analyzer::new(vec![Filter::Lowercase]);
        let found = terms_of(&definition(), Some(&analyzer), &record(text));
        (
            found.postings.iter().map(|(_, count)| *count).collect(),
            found.tokens,
        )
    }

    #[test]
    fn a_word_twice_is_one_posting_that_says_twice() {
        // The whole of what changed here: the terms are still deduplicated into
        // one posting each, but the run length is no longer thrown away on the
        // way. `dedup()` discarded exactly this number.
        let (frequencies, tokens) = analysed("lock lock contention");
        assert_eq!(frequencies.len(), 2, "two distinct terms");
        assert_eq!(frequencies.iter().sum::<u32>(), 3, "three tokens posted");
        assert!(frequencies.contains(&2), "the repeated term says 2");
        assert_eq!(tokens, 3, "length counts repeats");
    }

    #[test]
    fn every_term_of_a_text_with_no_repeats_says_once() {
        let (frequencies, tokens) = analysed("lock contention here");
        assert_eq!(frequencies, vec![1, 1, 1]);
        assert_eq!(tokens, 3);
    }

    #[test]
    fn the_frequency_is_taken_after_the_filters_and_not_before() {
        // `Lock` and `lock` are one term once lowercased, so they are one posting
        // with a frequency of two. Counting before the filters would report two
        // postings of one, which is the same mistake as scoring the spelling
        // rather than the word.
        let (frequencies, tokens) = analysed("Lock lock");
        assert_eq!(frequencies, vec![2]);
        assert_eq!(tokens, 2);
    }

    #[test]
    fn a_field_with_no_analyzer_posts_nothing_and_has_no_length() {
        let found = terms_of(&definition(), None, &record("lock contention"));
        assert!(found.postings.is_empty());
        assert_eq!(found.tokens, 0);
    }

    #[test]
    fn the_length_a_posting_states_saturates_rather_than_wrapping() {
        // A wrapped length would make one absurd record's score wrong by an
        // arbitrary amount while every other record still looked right.
        let mut found = terms_of(&definition(), None, &record(""));
        found.tokens = u64::from(u32::MAX) + 1;
        assert_eq!(found.length(), u32::MAX);
    }
}
