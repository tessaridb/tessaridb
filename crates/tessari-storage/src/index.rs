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

use tessari_constants::SPATIAL_INDEX_CELLS_PER_RECORD;
use tessari_encoding::{
    IndexAddress, IndexTarget, IndexValues, KeyKind, LogRecord, Mutation, NoPayload, Posting,
    PostingKey, RecordValue, SearchStatistics, SearchStatisticsKey, SearchTermKey,
    SecondaryIndexKey, SpatialExtent, SpatialIndexKey, StoreKey, StoreValue, TermStatistics,
    UniqueIndexKey, decode_payload,
};
use tessari_geo::{Bounds, Cell, Shape};
use tessari_kv::{Key, KeyRange, Keyspace, ScanDirection, ScanRequest, WriteBatch, WriteOp};
use tessari_types::{Analyzer, DatabaseId, NamespaceId, RecordId, TableId, Value};

use crate::catalog::{Catalog, IndexDefinition, defined_index};
use crate::covering;
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
pub(crate) fn maintain(
    store: &Store,
    record: &LogRecord,
    mut batch: WriteBatch,
) -> Result<WriteBatch> {
    let mut view = store.begin()?;
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
    settle(store, batch, &pending)
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
    pending.terms.insert(address, BTreeMap::new());
    let mut rows: BTreeMap<RecordId, Vec<u8>> = view
        .sweep_table(definition.namespace, definition.database, definition.table)?
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
        // Measured here and nowhere else: this is the one place the whole graph
        // and every stored vector are in hand at once, and it is reached by
        // applying a log record, so every replica computes the same figure.
        let batch = graph::write(batch, &address, &written);
        return Ok(graph::measure(batch, &address, &graph));
    }

    if definition.spatial {
        // Measured here and nowhere else, for the reason the vector branch above
        // states: this is the one place every geometry and its covering are in
        // hand at once, and it is reached by applying a log record, so every
        // replica computes the same figure. A figure accumulated from real reads
        // would differ per replica by construction.
        let mut placed = Vec::with_capacity(rows.len());
        for (id, payload) in &rows {
            let held = decode_payload(payload)?;
            if let Some((bounds, cells)) = covering_of(definition, &held) {
                batch = place_cells(batch, &address, id, bounds, &cells);
                placed.push(covering::Placed {
                    id: id.clone(),
                    bounds,
                    cells,
                });
            }
        }
        return Ok(covering::measure(batch, &address, &placed));
    }

    if definition.search {
        let declared = analyzers_on(view, definition.table)?;
        let analyzer = search_analyzer(definition, &declared);
        let mut counted = Delta::default();
        let mut dictionary: BTreeMap<IndexValues, Moved> = BTreeMap::new();
        for (id, payload) in &rows {
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
                    PostingKey::new(address, term, id.clone()).encode(),
                    Posting::Counted { frequency, length }.encode(),
                );
            }
        }
        *pending.moved.entry(address).or_default() = counted;
        pending.terms.insert(address, dictionary);
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
/// All eight index key kinds, because a rebuild has to be safe on any index a
/// caller may name, and an index whose shape changed is not a case this store
/// wants to reason about one kind at a time.
///
/// The measurements are cleared with the entries for a reason worth stating: a
/// recall or a refinement figure left behind would describe a graph or a
/// covering that no longer exists, which is exactly the stale number the
/// measurements were introduced to prevent, arriving from inside. Neither fails
/// a test until somebody reads it.
fn clear(store: &Store, mut batch: WriteBatch, address: &IndexAddress) -> Result<WriteBatch> {
    for kind in [
        KeyKind::SecondaryIndex,
        KeyKind::UniqueIndex,
        KeyKind::Posting,
        KeyKind::VectorNode,
        KeyKind::SearchStatistics,
        KeyKind::SpatialIndex,
        KeyKind::VectorRecall,
        KeyKind::SpatialRefinement,
        KeyKind::SearchTerm,
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
        if let RecordValue::Present(payload) = &mutation.value {
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
        if let RecordValue::Present(payload) = &mutation.value {
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
    //
    // Reading committed state alone could not see **this** transaction's own
    // removal, so a batch that deleted the record holding a value and then wrote
    // another record with it was refused by the index it was maintaining —
    // naming, as the offender, a record the same batch was about to delete. The
    // batch is asked instead. Ordering settles the rest: the delete was queued
    // first and this put is queued last, so the entry the batch leaves behind is
    // this one.
    //
    // The concurrency guarantee is untouched, because the precondition below is
    // still the committed entry: a second transaction that claims the value
    // first still wins and this batch still fails.
    let batch = match store.backend().get(keyspace, &key)? {
        Some(existing) => {
            if !releases(&batch, keyspace, &key)
                && IndexTarget::decode(existing.as_slice())?.id != *id
            {
                return Err(violation());
            }
            batch.expect_value(keyspace, key.clone(), existing)
        }
        None => batch.expect_absent(keyspace, key.clone()),
    };
    Ok(batch.put(keyspace, key, IndexTarget::new(id.clone()).encode()))
}

/// Whether this batch already deletes an index entry.
///
/// Asked per key, never "does the batch delete anything": a transaction that
/// releases one unique value has not released every one of them.
fn releases(batch: &WriteBatch, keyspace: Keyspace, key: &Key) -> bool {
    batch.ops().iter().any(|op| match op {
        WriteOp::Delete {
            keyspace: space,
            key: dropped,
        } => *space == keyspace && dropped == key,
        WriteOp::Put { .. } => false,
    })
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
    /// One entry per **distinct** term, with the number of times it occurs.
    ///
    /// Still one posting per distinct term — a duplicate key would be written
    /// twice to say the same thing — but the count is no longer discarded on the
    /// way there. It is what a relevance score means by *how often*, and it can
    /// only be taken here, from the same analyzer pass that produced the terms.
    postings: Vec<(IndexValues, u32)>,
    /// How many tokens the text holds, **with** repeats — this is a length, and
    /// a length that collapsed repeats would not be one.
    tokens: u64,
}

impl Analysed {
    /// The record's length as a posting states it.
    ///
    /// Narrowed rather than cast: a document of more than four billion tokens
    /// saturates, and a saturated length makes a score slightly wrong for one
    /// absurd record where a wrapped one would make it wrong by an arbitrary
    /// amount for that record and correct-looking for every other.
    fn length(&self) -> u32 {
        u32::try_from(self.tokens).unwrap_or(u32::MAX)
    }
}

/// Every cell of one record's geometry, written into `batch`.
///
/// The record's bounding box travels in each entry's value, and it is computed
/// **here** — inside the batch that carries the record's own mutation, by the
/// writer, once. Nothing else may compute it: a box maintained by a background
/// job or recomputed by a reader can lag the geometry it describes, and a stale
/// box excludes rows that should have matched with nothing raised anywhere. That
/// is the one failure direction a spatial filter must not have, and keeping the
/// computation on this path is the whole of the defence.
fn place(
    batch: WriteBatch,
    address: &IndexAddress,
    id: &RecordId,
    value: &Value,
    definition: &IndexDefinition,
) -> WriteBatch {
    let Some((bounds, cells)) = covering_of(definition, value) else {
        return batch;
    };
    place_cells(batch, address, id, bounds, &cells)
}

/// The write half of [`place`], for a caller that already holds the covering.
///
/// Split out so a build can write the entries and measure them from **one**
/// computed covering rather than two. Computing it twice would cost a second
/// pass and, worse, would let the entries and the figure describing them be
/// derived from separately-computed cells.
fn place_cells(
    mut batch: WriteBatch,
    address: &IndexAddress,
    id: &RecordId,
    bounds: Bounds,
    cells: &[Cell],
) -> WriteBatch {
    let extent = SpatialExtent::new(bounds).encode();
    for cell in cells {
        batch = batch.put(
            SpatialIndexKey::keyspace(),
            SpatialIndexKey::new(*address, *cell, id.clone()).encode(),
            extent.clone(),
        );
    }
    batch
}

/// Every cell of one record's geometry, deleted from `batch`.
fn displace(
    mut batch: WriteBatch,
    address: &IndexAddress,
    id: &RecordId,
    value: &Value,
    definition: &IndexDefinition,
) -> WriteBatch {
    let Some((_, cells)) = covering_of(definition, value) else {
        return batch;
    };
    for cell in cells {
        batch = batch.delete(
            SpatialIndexKey::keyspace(),
            SpatialIndexKey::new(*address, cell, id.clone()).encode(),
        );
    }
    batch
}

/// The box around one record's geometry, and the cells covering that box.
///
/// `None` when the field is absent or holds something that is not a geometry —
/// the same "not in this index at all" answer an ordered index gives for a
/// missing field, and the same answer a scan gives for the same record.
///
/// A geometry that will not lower to the grid gets the same answer, and cannot
/// arise for a stored record: positions are snapped at ingest and a geometry off
/// the sphere is refused there rather than here. Returning "not indexed" for it
/// keeps this function total without inventing a second refusal path for a case
/// the write path has already closed.
///
/// The cell count is bounded by [`SPATIAL_INDEX_CELLS_PER_RECORD`], and the
/// covering keeps a coarser cell rather than dropping a finer one when that
/// bound is reached — so exceeding the budget costs candidates to refine and
/// never rows.
fn covering_of(definition: &IndexDefinition, value: &Value) -> Option<(Bounds, Vec<Cell>)> {
    let path = definition.fields.first()?;
    let Value::Geometry(geometry) = path.resolve(value)? else {
        return None;
    };
    let bounds = Shape::of(geometry).ok()?.bounds()?;
    // The class each cell comes with is discarded, and only here: it says
    // whether the cell lies wholly inside the box it was produced for, which for
    // a *record's* own box tells a reader nothing. It is a query-side fact — the
    // reader's box is what decides whether a candidate may skip the predicate —
    // so keeping it in the entry would store an answer to a question nobody has
    // asked yet.
    let cells = tessari_geo::covering(bounds, SPATIAL_INDEX_CELLS_PER_RECORD)
        .into_iter()
        .map(|(cell, _)| cell)
        .collect();
    Some((bounds, cells))
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
    // Sorting puts equal terms next to each other, so a run *is* the count. This
    // replaces a `dedup()` that threw the run length away — the same pass, one
    // number further.
    Analysed {
        postings: terms
            .chunk_by(|held, next| held == next)
            .filter_map(|run| {
                let term = run.first()?;
                let frequency = u32::try_from(run.len()).unwrap_or(u32::MAX);
                Some((IndexValues::of(&[Value::from(term.as_str())]), frequency))
            })
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
fn settle(store: &Store, mut batch: WriteBatch, pending: &Pending) -> Result<WriteBatch> {
    let (moved, built) = (&pending.moved, &pending.built);
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
    settle_terms(store, batch, pending)
}

/// Fold the accumulated per-term deltas into the dictionary, one write per term
/// that moved.
///
/// # A term that reaches zero is deleted, not written as zero
///
/// The dictionary's whole purpose is that walking it enumerates the words the
/// index actually holds. An entry left behind at zero is a word a prefix walk
/// would return and whose posting list is empty — a suggestion nothing can
/// answer, offered by the structure built to stop exactly that. It also grows
/// without bound: every word ever written to the table stays in the dictionary
/// for the life of the store.
///
/// The read of the stored figure is skipped for an index this record **built**,
/// on the same reasoning [`settle`] gives for the collection statistics: a build
/// counted every row the index has, so its figure is a total and starting from
/// the stored one would count each document twice.
fn settle_terms(store: &Store, mut batch: WriteBatch, pending: &Pending) -> Result<WriteBatch> {
    let keyspace = SearchTermKey::keyspace();
    for (address, moved) in &pending.terms {
        let rebuilt = pending.built.contains(address);
        for (term, moved) in moved {
            // A rewrite that keeps a word nets a delta of zero and is still not
            // nothing: the word may now occur more often, or in a shorter
            // record, and the bound has to rise to cover it. Skipping on the
            // delta alone — which is what a count-only dictionary could do —
            // would leave a bound below a posting that exists, which is the one
            // direction ADR-0050 forbids.
            if moved.delta == 0 && moved.length.is_none() {
                continue;
            }
            let key = SearchTermKey::new(*address, term.clone()).encode();
            let held = if rebuilt {
                TermStatistics::default()
            } else {
                match store.backend().get(keyspace, &key)? {
                    Some(bytes) => TermStatistics::decode(bytes.as_slice())?,
                    None => TermStatistics::default(),
                }
            };
            let documents = shift(held.documents, moved.delta);
            batch = if documents == 0 {
                // The extremes leave with the entry, which is the one place they
                // are allowed to move inward. A term no record holds has no
                // postings to bound, so the next arrival starts from what it
                // actually writes rather than inheriting a ceiling from a record
                // that is gone.
                batch.delete(keyspace, key)
            } else {
                let frequency = held.max_frequency.max(moved.frequency);
                // `min` is not enough on its own, because zero is this field's
                // "never recorded" and would win every comparison — the sound
                // direction for a maximum and exactly backwards for a minimum.
                let length = match (held.min_length, moved.length) {
                    (0, arrived) => arrived.unwrap_or(0),
                    (held, Some(arrived)) => held.min(arrived),
                    (held, None) => held,
                };
                batch.put(
                    keyspace,
                    key,
                    TermStatistics::bounded(documents, frequency, length).encode(),
                )
            };
        }
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
