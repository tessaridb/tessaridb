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

use tessari_encoding::{
    IndexAddress, IndexTarget, IndexValues, KeyKind, LogRecord, Mutation, NoPayload, PostingKey,
    RecordValue, SearchStatistics, SearchStatisticsKey, SecondaryIndexKey, StoreKey, StoreValue,
    UniqueIndexKey, decode_payload,
};
use tessari_kv::{KeyRange, ScanDirection, ScanRequest, WriteBatch};
use tessari_types::{Analyzer, RecordId, TableId, Value};

use crate::catalog::{Catalog, IndexDefinition, defined_index};
use crate::error::{Error, Result};
use crate::graph;
use crate::store::Store;
use crate::transaction::{RecordAddress, Transaction};

/// What one log record accumulates while its index writes are built.
///
/// Every field is a fact a single mutation cannot see on its own: the unique
/// values already claimed *within this batch*, how each search index's
/// collection statistics have moved so far, and which indexes this record
/// builds outright. They travel together because they have the same lifetime
/// and the same reason to exist — the batch is the unit of atomicity, so it is
/// also the unit these are true of.
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
    /// Indexes [`build`] wrote whole in this record.
    ///
    /// Their statistics are a **total**, not a movement: the build counted every
    /// row the index has, so adding that to the stored figure would count each
    /// document a second time. `settle` reads this to know which of the two it
    /// is holding.
    built: BTreeSet<IndexAddress>,
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
    // question; see `tessari_types::Analyzer`.
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
    settle(store, batch, &pending.moved, &pending.built)
}

/// Every entry an index implies, written into the commit that defines — or
/// rebuilds — it.
///
/// The rows are the committed ones as of `view`, overlaid with this record's own
/// mutations for that table — so a script that defines an index and writes to the
/// table in one transaction indexes both what was there and what it just wrote.
///
/// # A build is authoritative, not additive
///
/// It makes the index's entries **be** what the rows imply, rather than adding
/// what the rows imply to whatever is already there. So it clears first, and a
/// search index's statistics come out as a total rather than a movement.
///
/// That is what a *rebuild* is, and it is why rebuilding needs no second path
/// and no new log shape: `REBUILD INDEX` writes the index's catalog record
/// again, unchanged, and a catalog record is an ordinary record (ADR-0009), so
/// this arrives exactly as a definition does. One rule covers both, and there
/// is no first-time-only branch left to be wrong.
///
/// A first build has nothing to clear and no statistics to reset, so the extra
/// work it pays for is four scans over an empty range — next to reading the
/// whole table, which it already does.
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
    batch = clear(store, batch, &address)?;
    pending.built.insert(address);
    // Anything the per-mutation pass moved for this index is discarded with the
    // entries it described: the rows below include this record's own mutations,
    // so counting them again is counting them twice.
    pending.moved.insert(address, Delta::default());
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

    if let Some(distance) = definition.vector {
        // The graph is built in the commit that defines the index, the same way
        // every other index is — so a definition over a populated table and a
        // definition over an empty one followed by writes reach the same state.
        let mut graph = graph::Graph::empty(distance);
        let mut written: BTreeMap<RecordId, tessari_encoding::VectorNode> = BTreeMap::new();
        for (id, payload) in &rows {
            let Some(held) = projected_vector(definition, &decode_payload(payload)?) else {
                continue;
            };
            written.extend(graph.insert(id, held));
        }
        return Ok(graph::write(batch, &address, &written));
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

    // A claim set of this build's own. The per-mutation pass may have claimed
    // values in this same index — and those claims describe entries the clear
    // above has just removed, so honouring them would refuse a rebuild for
    // colliding with itself. Nothing is lost: every row is seen here, so a
    // genuine duplicate still collides.
    let mut claimed = BTreeSet::new();
    for (id, payload) in &rows {
        for values in project(definition, &decode_payload(payload)?) {
            batch = insert(
                store,
                batch,
                definition,
                &address,
                &values,
                id,
                &mut claimed,
            )?;
        }
    }
    Ok(batch)
}

/// Every entry this index currently holds, deleted from the batch.
///
/// A build writes what the rows imply; without this it would leave behind
/// whatever an earlier build left — a node whose neighbour list points at
/// records that have gone, a posting for text nobody stores any more, an entry
/// under a value the record no longer holds.
///
/// All five index key kinds, because a rebuild has to be safe on any index a
/// caller may name, and an index whose shape changed is not a case this store
/// wants to reason about one kind at a time.
fn clear(store: &Store, mut batch: WriteBatch, address: &IndexAddress) -> Result<WriteBatch> {
    for kind in [
        KeyKind::SecondaryIndex,
        KeyKind::UniqueIndex,
        KeyKind::Posting,
        KeyKind::VectorNode,
        KeyKind::SearchStatistics,
    ] {
        let keyspace = kind.keyspace();
        let prefix = address.prefix(kind);
        let request = ScanRequest {
            keyspace,
            range: KeyRange::prefix(&prefix),
            direction: ScanDirection::Forward,
            limit: None,
        };
        for (key, _) in store.backend().scan(&request)? {
            batch = batch.delete(keyspace, key);
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
        if let RecordValue::Present(payload) = &mutation.value
            && let Some(held) = projected_vector(definition, &decode_payload(payload)?)
        {
            let touched = graph.insert(&mutation.id, held);
            batch = graph::write(batch, &address, &touched);
        }
        return Ok(batch);
    }

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

    if let RecordValue::Present(payload) = &mutation.value {
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

/// The vector one record contributes to a vector index.
///
/// `None` when the field is absent, holds something that is not an array of
/// numbers, or holds an empty one — the same "not in this index at all" answer
/// an ordered index gives for a missing field, and the same reading the
/// language's own distance functions do.
fn projected_vector(definition: &IndexDefinition, value: &Value) -> Option<Vec<f64>> {
    let path = definition.fields.first()?;
    graph::vector_of(path.resolve(value)?)
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
    built: &BTreeSet<IndexAddress>,
) -> Result<WriteBatch> {
    let keyspace = SearchStatisticsKey::keyspace();
    for (address, delta) in moved {
        if delta.is_zero() {
            continue;
        }
        let key = SearchStatisticsKey::new(*address).encode();
        // An index this record **built** was counted whole, so its figure is a
        // total and starts from nothing. One that was merely updated carries a
        // movement, and starts from what is stored. Reading the stored figure
        // for a build would count every document twice — silently, since a
        // collection statistic has no reader who would notice it drifting.
        let held = if built.contains(address) {
            SearchStatistics::default()
        } else {
            match store.backend().get(keyspace, &key)? {
                Some(bytes) => SearchStatistics::decode(bytes.as_slice())?,
                None => SearchStatistics::default(),
            }
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

/// The entries one record contributes to an index — none, one, or several.
///
/// An empty answer means "not in this index" rather than "indexed under
/// nothing": a record that is not an object has nothing to project, and a record
/// where one of the indexed routes reaches nothing has no value to place.
///
/// A route reaching nothing covers more ground than a missing field did: a
/// missing intermediate, an object addressed by position, an array addressed by
/// name. All of them are the same answer, and it is the same answer a missing
/// top-level field has always given, which is what lets documents of differing
/// shapes share a table without the index having an opinion about it.
///
/// # Several, and why the general shape is the simpler one to be right about
///
/// A route holding `[*]` reaches several values, and the record contributes one
/// entry per value — which is what a multikey index *is*. Written as a product
/// over the routes, so a route with no `[*]` contributes exactly the one value it
/// contributes today and the composite case needs no second rule. `DEFINE INDEX`
/// admits at most one multi-valued route, so the product never actually
/// multiplies; the code does not need to know that, and a version that did would
/// be longer and would have a branch nothing exercises.
///
/// Entries are **deduplicated**, so `tags: ['dup', 'dup']` is one entry rather
/// than an entry written twice. The batch would make that idempotent anyway; the
/// point is that the remove side runs this same function, and two sides that
/// agree by construction cannot leave an orphan behind.
pub(crate) fn project(definition: &IndexDefinition, value: &Value) -> Vec<IndexValues> {
    let mut rows: Vec<Vec<Value>> = vec![Vec::with_capacity(definition.fields.len())];
    for path in &definition.fields {
        let reached: Vec<&Value> = if path.is_several() {
            path.reach(value)
        } else {
            match path.resolve(value) {
                Some(Value::None) | None => return Vec::new(),
                Some(found) => vec![found],
            }
        };
        if reached.is_empty() {
            return Vec::new();
        }
        rows = rows
            .iter()
            .flat_map(|held| {
                reached.iter().map(|found| {
                    let mut next = held.clone();
                    next.push((*found).clone());
                    next
                })
            })
            .collect();
    }
    rows.iter()
        .map(|held| IndexValues::of(held))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}
