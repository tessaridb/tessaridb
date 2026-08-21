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

use std::collections::{BTreeMap, BTreeSet};

use bgv_db_encoding::{
    IndexAddress, IndexTarget, IndexValues, LogRecord, Mutation, NoPayload, PostingKey,
    RecordValue, SearchStatistics, SearchStatisticsKey, SecondaryIndexKey, StoreKey, StoreValue,
    UniqueIndexKey, decode_payload,
};
use bgv_db_kv::WriteBatch;
use bgv_db_types::{Analyzer, RecordId, TableId, Value};

use crate::catalog::{Catalog, IndexDefinition, defined_index};
use crate::error::{Error, Result};
use crate::store::Store;
use crate::transaction::{RecordAddress, Transaction};

/// What one log record accumulates while its index writes are built.
///
/// Both fields are facts a single mutation cannot see on its own: the unique
/// values already claimed *within this batch*, and how each search index's
/// collection statistics have moved so far. They travel together because they
/// have the same lifetime and the same reason to exist — the batch is the unit
/// of atomicity, so it is also the unit these are true of.
#[derive(Debug, Default)]
struct Pending {
    /// Unique index keys this batch has already written.
    ///
    /// Two records in ONE batch claiming one unique value would each find the
    /// key absent and each write it, and the second would silently overwrite the
    /// first. A precondition cannot catch that — both are satisfied.
    claimed: BTreeSet<Vec<u8>>,
    /// How each search index's statistics move, written once at the end.
    moved: BTreeMap<IndexAddress, Delta>,
}

/// Add the index writes a log record implies to `batch`.
///
/// Reads the catalog and the records' current values as of the committed state,
/// which is the state this record is about to be applied on top of.
pub(crate) fn maintain(
    store: &Store,
    record: &LogRecord,
    mut batch: WriteBatch,
) -> Result<WriteBatch> {
    let mut view = store.begin()?;
    let mut by_table: BTreeMap<TableId, Vec<IndexDefinition>> = BTreeMap::new();
    // The analyzer a search index uses is the **field's** declaration, not the
    // index's, so it is read from the schema here — once per table rather than
    // once per record. That is what makes a scan and an index answer the same
    // question; see `bgv_db_types::Analyzer`.
    let mut analyzers: BTreeMap<TableId, BTreeMap<String, Analyzer>> = BTreeMap::new();
    let mut pending = Pending::default();

    for mutation in record.mutations() {
        let definitions = match by_table.get(&mutation.table) {
            Some(found) => found.clone(),
            None => {
                let found = Catalog::new(&mut view).indexes_on(mutation.table)?;
                by_table.insert(mutation.table, found.clone());
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
        let previous = view.get(&address)?;

        for definition in &definitions {
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
    settle(store, batch, &pending.moved)
}

/// Every entry a newly defined index implies, added to the commit that defines
/// it.
///
/// The rows are the committed ones as of `view`, overlaid with this record's own
/// mutations for that table — so a script that defines an index and writes to the
/// table in one transaction indexes both what was there and what it just wrote.
fn build(
    store: &Store,
    mut batch: WriteBatch,
    view: &mut Transaction<'_>,
    record: &LogRecord,
    definition: &IndexDefinition,
    pending: &mut Pending,
) -> Result<WriteBatch> {
    let address = IndexAddress::new(
        definition.namespace,
        definition.database,
        definition.table,
        definition.id,
    );
    let mut rows: BTreeMap<RecordId, Vec<u8>> = view
        .scan_table(definition.namespace, definition.database, definition.table)?
        .into_iter()
        .collect();

    for mutation in record.mutations() {
        if mutation.namespace != definition.namespace
            || mutation.database != definition.database
            || mutation.table != definition.table
        {
            continue;
        }
        match &mutation.value {
            RecordValue::Present(payload) => rows.insert(mutation.id.clone(), payload.clone()),
            RecordValue::Tombstone => rows.remove(&mutation.id),
        };
    }

    if definition.search {
        let declared = analyzers_on(view, definition.table)?;
        let analyzer = search_analyzer(definition, &declared);
        let counted = pending.moved.entry(address).or_default();
        for (id, payload) in &rows {
            let analysed = terms_of(definition, analyzer, &decode_payload(payload)?);
            counted.added(analysed.tokens);
            for term in analysed.postings {
                batch = batch.put(
                    PostingKey::keyspace(),
                    PostingKey::new(address, term, id.clone()).encode(),
                    NoPayload.encode(),
                );
            }
        }
        return Ok(batch);
    }

    for (id, payload) in &rows {
        if let Some(values) = project(definition, &decode_payload(payload)?) {
            batch = insert(
                store,
                batch,
                definition,
                &address,
                &values,
                id,
                &mut pending.claimed,
            )?;
        }
    }
    Ok(batch)
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

    if definition.search {
        let analyzer = search_analyzer(definition, analyzers);
        let counted = pending.moved.entry(address).or_default();
        // The old side first, and both sides of the same change: a record whose
        // text changed leaves the index at its former length and re-enters at
        // its new one, so a statistic that only counted arrivals would drift
        // upward by exactly the amount nobody ever looks at.
        if let Some(bytes) = previous {
            let analysed = terms_of(definition, analyzer, &decode_payload(bytes)?);
            counted.removed(analysed.tokens);
            for term in analysed.postings {
                batch = batch.delete(
                    PostingKey::keyspace(),
                    PostingKey::new(address, term, mutation.id.clone()).encode(),
                );
            }
        }
        if let RecordValue::Present(payload) = &mutation.value {
            let analysed = terms_of(definition, analyzer, &decode_payload(payload)?);
            counted.added(analysed.tokens);
            for term in analysed.postings {
                batch = batch.put(
                    PostingKey::keyspace(),
                    PostingKey::new(address, term, mutation.id.clone()).encode(),
                    NoPayload.encode(),
                );
            }
        }
        return Ok(batch);
    }

    // The old entry goes first: a record whose indexed value changed must not
    // leave the entry that pointed at its former value behind, and an entry
    // nothing will ever reconcile is the failure mode secondary indexes are
    // known for.
    if let Some(bytes) = previous {
        if let Some(values) = project(definition, &decode_payload(bytes)?) {
            batch = remove(batch, definition, &address, &values, &mutation.id);
        }
    }

    if let RecordValue::Present(payload) = &mutation.value {
        if let Some(values) = project(definition, &decode_payload(payload)?) {
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

fn insert(
    store: &Store,
    batch: WriteBatch,
    definition: &IndexDefinition,
    address: &IndexAddress,
    values: &IndexValues,
    id: &RecordId,
    claimed: &mut BTreeSet<Vec<u8>>,
) -> Result<WriteBatch> {
    if !definition.unique {
        let key = SecondaryIndexKey::new(*address, values.clone(), id.clone());
        return Ok(batch.put(
            SecondaryIndexKey::keyspace(),
            key.encode(),
            NoPayload.encode(),
        ));
    }

    let keyspace = UniqueIndexKey::keyspace();
    let key = UniqueIndexKey::new(*address, values.clone()).encode();
    let violation = || Error::UniqueViolation {
        index: definition.name.clone(),
        id: id.clone(),
    };
    if !claimed.insert(key.as_slice().to_vec()) {
        return Err(violation());
    }

    // The precondition is what makes this safe against a concurrent writer: the
    // check below reads committed state, and without it a second transaction
    // could claim the value between the read and the apply.
    let batch = match store.backend().get(keyspace, &key)? {
        Some(existing) => {
            if IndexTarget::decode(existing.as_slice())?.id != *id {
                return Err(violation());
            }
            batch.expect_value(keyspace, key.clone(), existing)
        }
        None => batch.expect_absent(keyspace, key.clone()),
    };
    Ok(batch.put(keyspace, key, IndexTarget::new(id.clone()).encode()))
}

fn remove(
    batch: WriteBatch,
    definition: &IndexDefinition,
    address: &IndexAddress,
    values: &IndexValues,
    id: &RecordId,
) -> WriteBatch {
    if definition.unique {
        let key = UniqueIndexKey::new(*address, values.clone());
        batch.delete(UniqueIndexKey::keyspace(), key.encode())
    } else {
        let key = SecondaryIndexKey::new(*address, values.clone(), id.clone());
        batch.delete(SecondaryIndexKey::keyspace(), key.encode())
    }
}

/// The analyzer each analysed field of a table declares, by field name.
fn analyzers_on(view: &mut Transaction<'_>, table: TableId) -> Result<BTreeMap<String, Analyzer>> {
    let declared = Catalog::new(view).fields_on(table)?;
    let wanted: BTreeMap<String, String> = declared
        .into_iter()
        .filter_map(|field| field.analyzer.map(|named| (field.name, named)))
        .collect();
    if wanted.is_empty() {
        return Ok(BTreeMap::new());
    }
    let mut resolved = BTreeMap::new();
    for definition in Catalog::new(view).analyzers()? {
        for (field, named) in &wanted {
            if *named == definition.name {
                resolved.insert(field.clone(), definition.analyzer.clone());
            }
        }
    }
    Ok(resolved)
}

/// The analyzer a search index reads its field's text with.
fn search_analyzer<'a>(
    definition: &IndexDefinition,
    analyzers: &'a BTreeMap<String, Analyzer>,
) -> Option<&'a Analyzer> {
    definition
        .fields
        .first()
        .and_then(|path| analyzers.get(path.root()))
}

/// What one record contributes to a search index: its postings, and its length.
///
/// Both come out of a **single** analyzer pass. They could each be computed on
/// their own, and then a change to the tokenizer would have to reach two places
/// to keep the postings and the statistics describing the same text — which is
/// the shape of drift that gets noticed as a ranking that is subtly wrong.
#[derive(Debug, Default)]
struct Analysed {
    /// One entry per **distinct** term: a word twice in one document is one
    /// posting, because the question a posting answers is membership and a
    /// duplicate key would be written twice to say the same thing.
    postings: Vec<IndexValues>,
    /// How many tokens the text holds, **with** repeats — this is a length, and
    /// a length that collapsed repeats would not be one.
    tokens: u64,
}

/// The terms one record contributes to a search index.
///
/// Empty when the field declares no analyzer, holds no text, or the record does
/// not have it — the same "not in this index at all" answer an ordered index
/// gives, and the same answer a scan gives for the same record.
fn terms_of(definition: &IndexDefinition, analyzer: Option<&Analyzer>, value: &Value) -> Analysed {
    let (Some(analyzer), Some(path)) = (analyzer, definition.fields.first()) else {
        return Analysed::default();
    };
    let Some(Value::String(text)) = path.resolve(value) else {
        return Analysed::default();
    };
    let mut terms: Vec<String> = analyzer.terms(text);
    let tokens = u64::try_from(terms.len()).unwrap_or(u64::MAX);
    terms.sort_unstable();
    terms.dedup();
    Analysed {
        postings: terms
            .into_iter()
            .map(|term| IndexValues::of(&[Value::from(term.as_str())]))
            .collect(),
        tokens,
    }
}

/// How one log record moves an index's collection statistics.
///
/// Signed, and accumulated rather than written per mutation: a batch touching
/// one index a thousand times moves two counters a thousand times and writes
/// them once.
#[derive(Debug, Clone, Copy, Default)]
struct Delta {
    documents: i64,
    tokens: i64,
}

impl Delta {
    /// Record a document leaving the index at this length.
    fn removed(&mut self, tokens: u64) {
        if tokens == 0 {
            return;
        }
        self.documents = self.documents.saturating_sub(1);
        self.tokens = self
            .tokens
            .saturating_sub(i64::try_from(tokens).unwrap_or(i64::MAX));
    }

    /// Record a document entering the index at this length.
    fn added(&mut self, tokens: u64) {
        if tokens == 0 {
            return;
        }
        self.documents = self.documents.saturating_add(1);
        self.tokens = self
            .tokens
            .saturating_add(i64::try_from(tokens).unwrap_or(i64::MAX));
    }

    /// Whether anything moved.
    const fn is_zero(self) -> bool {
        self.documents == 0 && self.tokens == 0
    }
}

/// Fold the accumulated deltas into the stored statistics, one write per index.
///
/// The current values are read from **committed** state, which is the state this
/// log record is about to be applied on top of — the same state the previous
/// record values above were read at, so the two cannot describe different
/// moments. A store with one writer (ADR-0007) makes that read-modify-write safe
/// without a counter primitive.
fn settle(
    store: &Store,
    mut batch: WriteBatch,
    moved: &BTreeMap<IndexAddress, Delta>,
) -> Result<WriteBatch> {
    let keyspace = SearchStatisticsKey::keyspace();
    for (address, delta) in moved {
        if delta.is_zero() {
            continue;
        }
        let key = SearchStatisticsKey::new(*address).encode();
        let held = match store.backend().get(keyspace, &key)? {
            Some(bytes) => SearchStatistics::decode(bytes.as_slice())?,
            None => SearchStatistics::default(),
        };
        let updated = SearchStatistics::new(
            shift(held.documents, delta.documents),
            shift(held.terms, delta.tokens),
        );
        batch = batch.put(keyspace, key, updated.encode());
    }
    Ok(batch)
}

/// A count moved by a signed amount, without wrapping below zero.
fn shift(count: u64, delta: i64) -> u64 {
    if delta.is_negative() {
        count.saturating_sub(delta.unsigned_abs())
    } else {
        count.saturating_add(delta.unsigned_abs())
    }
}

/// The indexed values of one record, or `None` when the record is not indexed.
///
/// A record that is not an object has nothing to project, and a record where one
/// of the indexed paths reaches nothing has no value to place — both mean "not
/// in this index" rather than "indexed under nothing".
///
/// A path reaching nothing covers more ground than a missing field did: a
/// missing intermediate, an object addressed by position, an array addressed by
/// name. All of them are the same answer, and it is the same answer a missing
/// top-level field has always given, which is what lets documents of differing
/// shapes share a table without the index having an opinion about it.
pub(crate) fn project(definition: &IndexDefinition, value: &Value) -> Option<IndexValues> {
    let mut projected = Vec::with_capacity(definition.fields.len());
    for path in &definition.fields {
        match path.resolve(value) {
            Some(Value::None) | None => return None,
            Some(found) => projected.push(found.clone()),
        }
    }
    Some(IndexValues::of(&projected))
}
