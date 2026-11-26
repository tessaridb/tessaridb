//! The pruning scan (SG3 T3.2): chunks examined against their head
//! statistics, survivors decoded, the predicate re-evaluated per row.
//!
//! # Filter-and-refine, never believe (the design record, OLAP skill M3)
//!
//! A chunk the statistics say cannot match is **skipped**; every other chunk
//! is decoded and its rows are tested against the real predicate. The hits
//! are **candidates, not results**: a tombstoned record's row still sits in
//! its chunk (the design record — delete writes no chunk rewrite), a bloom says
//! "maybe" by design, and the projection describes only what a commit
//! wrote. The caller above this scan owns the row authority: it re-checks
//! each candidate against the record at its own snapshot before anything
//! answers. Exactness never depends on this scan being right; it depends on
//! the scan never *denying* a row that matches — which the write-path
//! invariant underneath it guarantees (§4.4: the statistics were written in
//! the same commit as the data, by the one writer).
//!
//! # Pruning rules
//!
//! - `=` — the bloom denies, or else the constant sits outside [min, max].
//! - `<, <=, >, >=` — min/max prove no row can qualify.
//! - `!=` — never prunes: a bloom miss does not disprove inequality.
//! - Absent statistics (a chunk written before this wave, or a disabled
//!   range) make the chunk a candidate; absent disables pruning, it does
//!   not enable guessing.
//! - Null slots never satisfy a comparison with a non-null constant, so a
//!   chunk pruned on non-null statistics loses nothing.
//!
//! `prune: false` decodes everything — the suite's equivalence arm runs one
//! scan both ways and asserts identical hit sets (the only proof a skipping
//! structure is not silently dropping rows; OLAP skill M35).
//!
//! # What the counters mean
//!
//! `chunks_examined`/`chunks_skipped` are per family and summed;
//! `rows_decoded` counts decoded rows across the survivors;
//! `rows_returned` is the size of the final candidate set after every
//! family's predicate has intersected.

use std::collections::BTreeSet;

use tessari_encoding::{ColumnFamily, PointerKey, encode_payload};
use tessari_types::{DatabaseId, NamespaceId, RecordId, TableId, Value};

use crate::error::Result;
use crate::store::Store;

use super::chunk::{decode_chunk, decode_head};
use super::project::newest_under;

/// The comparison one predicate applies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColumnarCompare {
    /// `field = value`
    Equal,
    /// `field != value` — never prunes.
    NotEqual,
    /// `field < value`
    Less,
    /// `field <= value`
    LessOrEqual,
    /// `field > value`
    Greater,
    /// `field >= value`
    GreaterOrEqual,
}

/// One family's predicate: the projection of `family` tested against
/// constants the caller evaluated and bound.
#[derive(Clone, Debug)]
pub struct ColumnarPredicate {
    /// The projected family under test.
    pub family: ColumnFamily,
    /// The test the surviving rows must pass.
    pub test: ColumnarTest,
}

/// The test one predicate applies.
#[derive(Clone, Debug)]
pub enum ColumnarTest {
    /// One comparison against one bound constant.
    Compare {
        /// The comparison.
        compare: ColumnarCompare,
        /// The constant, evaluated and bound by the caller. A null constant
        /// never prunes and never matches here; the row authority above
        /// decides null semantics for real.
        value: Value,
    },
    /// Equality against any of the bound constants (`field IN (…)`). A null
    /// member never matches and contributes nothing; the caller above the
    /// storage layer declines lists carrying one.
    In {
        /// The constants, evaluated and bound by the caller.
        values: Vec<Value>,
    },
}

/// What the scan did — the the design record counters the answer reports.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ColumnarScanReport {
    /// Chunks whose statistics were consulted (per family, summed).
    pub chunks_examined: u64,
    /// Chunks the statistics proved cannot match (per family, summed).
    pub chunks_skipped: u64,
    /// Rows decoded from surviving chunks (per family, summed).
    pub rows_decoded: u64,
    /// Candidates handed up after every predicate intersected.
    pub rows_returned: u64,
}

/// Scan a columnar table's projection: every family's chunks, pruned by
/// their head statistics, intersected across `predicates`.
///
/// The returned ids are candidates in no promised order; the caller owns
/// ordering (the row walk's contract is record-key order) and the row
/// authority. `predicates` must not be empty — an empty conjunction is the
/// whole table, which is what the row walk is for.
///
/// # Errors
///
/// A chunk whose bytes refuse to parse is an error, not a skip: a corrupt
/// projection is a defect the row authority cannot cover, because the
/// candidates it would have named are gone.
pub async fn columnar_scan(
    store: &Store,
    namespace: NamespaceId,
    database: DatabaseId,
    table: TableId,
    predicates: &[ColumnarPredicate],
    prune: bool,
) -> Result<(Vec<RecordId>, ColumnarScanReport)> {
    columnar_scan_partitioned(store, namespace, database, table, predicates, prune, None).await
}

/// The pruning scan over one partition's lane, when the caller pins one —
/// a predicate the session gate proved fixes the partition key to one
/// constant routes here (the design record: a partition's data is one prefix,
/// and a read naming one partition reads one lane).
///
/// # Errors
///
/// As [`columnar_scan`]; a pin naming a lane the family's pointer does not
/// hold answers empty, which is exact — no row routed there.
pub async fn columnar_scan_partitioned(
    store: &Store,
    namespace: NamespaceId,
    database: DatabaseId,
    table: TableId,
    predicates: &[ColumnarPredicate],
    prune: bool,
    pin: Option<u64>,
) -> Result<(Vec<RecordId>, ColumnarScanReport)> {
    let mut report = ColumnarScanReport::default();
    let mut hits: Option<BTreeSet<RecordId>> = None;
    for predicate in predicates {
        let family_hits = scan_family(
            store,
            namespace,
            database,
            table,
            predicate,
            prune,
            pin,
            &mut report,
        )
        .await?;
        hits = Some(match hits {
            None => family_hits,
            Some(existing) => {
                let merged: BTreeSet<RecordId> =
                    existing.intersection(&family_hits).cloned().collect();
                report.rows_returned = u64::try_from(merged.len()).unwrap_or(u64::MAX);
                merged
            }
        });
    }
    let candidates = hits.unwrap_or_default();
    if predicates.len() <= 1 {
        report.rows_returned = u64::try_from(candidates.len()).unwrap_or(u64::MAX);
    }
    Ok((candidates.into_iter().collect(), report))
}

/// Every decoded row of one family's one lane, in local chunk order:
/// `(record id, value)` pairs, null slots omitted (a null or absent value
/// holds no row, the design record). This is the partition-local group-by's raw
/// read: the caller unions the ids across the families it folds, so a
/// record whose key is null — which routed here but holds no key-family
/// row — is not lost.
///
/// # Errors
///
/// As [`columnar_scan`]; a lane the family's pointer does not hold answers
/// empty.
pub async fn columnar_lane(
    store: &Store,
    namespace: NamespaceId,
    database: DatabaseId,
    table: TableId,
    family: &ColumnFamily,
    partition: u64,
) -> Result<Vec<(RecordId, Value)>> {
    let lanes = match newest_under(
        store,
        &PointerKey::versions_prefix(namespace, database, table, family),
    )
    .await?
    {
        Some((_, bytes)) => super::project::lanes_from_bytes(&bytes)?,
        None => return Ok(Vec::new()),
    };
    let Some(open) = lanes.get(&partition).map(|lane| lane.open) else {
        return Ok(Vec::new());
    };
    let mut rows = Vec::new();
    let mut local = 0_u64;
    while local <= super::chunk::local_of_seq(open) {
        if let Some((_, bytes)) = newest_under(
            store,
            &tessari_encoding::ChunkKey::versions_prefix(
                namespace,
                database,
                table,
                family,
                super::chunk::split_seq(partition, local),
            ),
        )
        .await?
        {
            let chunk = decode_chunk(&bytes)?;
            rows.extend(chunk.record_ids.into_iter().zip(chunk.values));
        }
        local = local.saturating_add(1);
    }
    Ok(rows)
}

/// One family's chunks: prune by the head statistics, decode survivors,
/// test each row.
#[allow(clippy::too_many_arguments)]
async fn scan_family(
    store: &Store,
    namespace: NamespaceId,
    database: DatabaseId,
    table: TableId,
    predicate: &ColumnarPredicate,
    prune: bool,
    pin: Option<u64>,
    report: &mut ColumnarScanReport,
) -> Result<BTreeSet<RecordId>> {
    let lanes = match newest_under(
        store,
        &PointerKey::versions_prefix(namespace, database, table, &predicate.family),
    )
    .await?
    {
        Some((_, bytes)) => super::project::lanes_from_bytes(&bytes)?,
        None => return Ok(BTreeSet::new()),
    };
    let mut hits = BTreeSet::new();
    for (partition, lane) in lanes {
        if pin.is_some_and(|pin| pin != partition) {
            continue;
        }
        // The lane head is the cheapest pruning level (the design record,
        // OLAP ref 02 M1): the pointer's range statistics describe every
        // sealed chunk of the lane, so a predicate outside them skips the
        // lane's sealed chunks without reading a single head. Absence
        // disables. The open chunk is never covered by the statistics —
        // appends continue into it — so it is consulted every time, denied
        // lane or not.
        let open = lane.open;
        let open_local = super::chunk::local_of_seq(open);
        let start = if prune && lane_denied(predicate, lane.range.as_ref()) {
            open_local
        } else {
            0
        };
        // The head index (ADR-0135 S13-B): when the lane's entries are
        // complete against this open seq, the chunks the predicate cannot
        // match are told apart in memory and only the rest are seeked. The
        // open chunk is outside the index and joins every visit, by the same
        // rule the lane statistics keep. A lane the index cannot vouch for
        // walks every head — the same loop, over every local — and installs
        // what it walked, so the next read of an unchanged lane seeks only
        // what can match.
        let key = super::heads::LaneKey {
            table,
            family: predicate.family.clone(),
            partition,
        };
        let current = if prune {
            store.heads().current(&key, open)
        } else {
            None
        };
        let mut walked: Option<super::heads::LaneHeads> = None;
        let visit: Vec<u64> = match &current {
            Some(heads) => {
                let mut visit: Vec<u64> = Vec::new();
                for (local, entry) in &heads.entries {
                    if stats_deny(predicate, &entry.stats) {
                        report.chunks_skipped = report.chunks_skipped.saturating_add(1);
                    } else {
                        visit.push(*local);
                    }
                }
                visit.push(open_local);
                visit
            }
            None => {
                // A walk installs what it walked — and only a walk that saw
                // every sealed chunk may claim completeness. A lane the lane
                // statistics already denied starts at the open chunk, so its
                // walk saw none of the sealed heads and installs nothing:
                // an empty set claiming to be the whole would have the next
                // read skip chunks that can match, which is the one failure
                // this index must never produce.
                if start == 0 {
                    walked = Some(super::heads::LaneHeads {
                        open_at: open,
                        entries: std::collections::BTreeMap::new(),
                    });
                }
                (start..=open_local).collect()
            }
        };
        for local in visit {
            let seq = super::chunk::split_seq(partition, local);
            if let Some((_, bytes)) = newest_under(
                store,
                &tessari_encoding::ChunkKey::versions_prefix(
                    namespace,
                    database,
                    table,
                    &predicate.family,
                    seq,
                ),
            )
            .await?
            {
                report.chunks_examined = report.chunks_examined.saturating_add(1);
                if let Some(collected) = walked.as_mut()
                    && local < open_local
                    && let Ok((_, stats)) = decode_head(&bytes)
                {
                    collected
                        .entries
                        .insert(local, super::heads::HeadEntry { stats });
                }
                if prune && pruned_by_head(predicate, &bytes)? {
                    report.chunks_skipped = report.chunks_skipped.saturating_add(1);
                } else {
                    let chunk = decode_chunk(&bytes)?;
                    report.rows_decoded = report
                        .rows_decoded
                        .saturating_add(u64::try_from(chunk.values.len()).unwrap_or(u64::MAX));
                    if prune && chunk.sorted && chunk.null_count == 0 {
                        // The writer recorded the values non-decreasing, so
                        // the test narrows to partition points instead of
                        // walking every row (the sound within-chunk half of
                        // the sort-key binary search). Nulls interleaved
                        // would break the vec order, so null-bearing chunks
                        // keep the linear path; the equivalence suite proves
                        // both answer the same set.
                        insert_sorted_hits(&mut hits, &predicate.test, chunk);
                    } else {
                        for (id, value) in chunk.record_ids.iter().zip(chunk.values.iter()) {
                            if matches(&predicate.test, value) {
                                hits.insert(id.clone());
                            }
                        }
                    }
                }
            }
        }
        if let Some(collected) = walked {
            store.heads().install(key, collected);
        }
    }
    Ok(hits)
}

/// Whether the lane's range statistics prove no sealed row of this lane can
/// match. Range only (no bloom at this level): a constant outside [min, max]
/// denies, and an IN-list is denied only when every member is outside. The
/// open chunk is never covered, so the scan still consults it.
fn lane_denied(predicate: &ColumnarPredicate, range: Option<&super::project::LaneRange>) -> bool {
    let Some(range) = range else { return false };
    let outside = |value: &Value| value < &range.min || value > &range.max;
    match &predicate.test {
        ColumnarTest::Compare { compare, value } => {
            if is_null(value) {
                return false;
            }
            match compare {
                ColumnarCompare::NotEqual => false,
                ColumnarCompare::Equal => outside(value),
                ColumnarCompare::Less => range.min >= *value,
                ColumnarCompare::LessOrEqual => range.min > *value,
                ColumnarCompare::Greater => range.max <= *value,
                ColumnarCompare::GreaterOrEqual => range.max < *value,
            }
        }
        ColumnarTest::In { values } => {
            !values.is_empty() && values.iter().all(|value| is_null(value) || outside(value))
        }
    }
}

/// Insert the rows of a decoded sorted chunk that pass the test, narrowed by
/// binary search over the values. The writer's `sorted` flag is the
/// invariant that makes this exact; the row authority above re-confirms
/// either way, and the equivalence suite proves the narrowed and linear
/// paths answer the same set.
fn insert_sorted_hits(
    hits: &mut BTreeSet<RecordId>,
    test: &ColumnarTest,
    chunk: super::chunk::ColumnChunk,
) {
    let values = &chunk.values;
    let emit = |hits: &mut BTreeSet<RecordId>, rows: std::ops::Range<usize>| {
        for row in rows {
            hits.insert(chunk.record_ids[row].clone());
        }
    };
    match test {
        ColumnarTest::Compare { compare, value } => match compare {
            ColumnarCompare::Equal => {
                let lo = values.partition_point(|held| held < value);
                let hi = values.partition_point(|held| held <= value);
                emit(hits, lo..hi);
            }
            ColumnarCompare::NotEqual => {
                let lo = values.partition_point(|held| held < value);
                let hi = values.partition_point(|held| held <= value);
                emit(hits, 0..lo);
                emit(hits, hi..values.len());
            }
            ColumnarCompare::Less => emit(hits, 0..values.partition_point(|held| held < value)),
            ColumnarCompare::LessOrEqual => {
                emit(hits, 0..values.partition_point(|held| held <= value));
            }
            ColumnarCompare::Greater => {
                emit(
                    hits,
                    values.partition_point(|held| held <= value)..values.len(),
                );
            }
            ColumnarCompare::GreaterOrEqual => {
                emit(
                    hits,
                    values.partition_point(|held| held < value)..values.len(),
                );
            }
        },
        ColumnarTest::In { values: constants } => {
            for constant in constants {
                if is_null(constant) {
                    continue;
                }
                let lo = values.partition_point(|held| held < constant);
                let hi = values.partition_point(|held| held <= constant);
                emit(hits, lo..hi);
            }
        }
    }
}

/// Whether the head statistics prove no row of this chunk can match.
fn pruned_by_head(predicate: &ColumnarPredicate, bytes: &[u8]) -> Result<bool> {
    let (_, stats) = decode_head(bytes)?;
    Ok(stats_deny(predicate, &stats))
}

/// The head test over statistics already in hand — the one test both the
/// head read and the in-memory head index apply, so the two cannot drift.
fn stats_deny(predicate: &ColumnarPredicate, stats: &super::chunk::ChunkStats) -> bool {
    match &predicate.test {
        ColumnarTest::Compare { compare, value } => {
            if is_null(value) {
                return false;
            }
            match compare {
                ColumnarCompare::NotEqual => false,
                ColumnarCompare::Equal => {
                    if let Some(bloom) = &stats.bloom {
                        return !bloom.might_contain(&encode_payload(value).into_bytes());
                    }
                    outside(stats, value)
                }
                ColumnarCompare::Less => stats.min.as_ref().is_some_and(|min| min >= value),
                ColumnarCompare::LessOrEqual => stats.min.as_ref().is_some_and(|min| min > value),
                ColumnarCompare::Greater => stats.max.as_ref().is_some_and(|max| max <= value),
                ColumnarCompare::GreaterOrEqual => {
                    stats.max.as_ref().is_some_and(|max| max < value)
                }
            }
        }
        // A list is denied only when every member is: outside the [min, max]
        // range AND denied by the bloom, each on its own.
        ColumnarTest::In { values } => {
            !values.is_empty()
                && values.iter().all(|value| {
                    is_null(value)
                        || (outside(stats, value)
                            && stats.bloom.as_ref().is_some_and(|bloom| {
                                !bloom.might_contain(&encode_payload(value).into_bytes())
                            }))
                })
        }
    }
}

/// Whether `value` sits outside [min, max].
fn outside(stats: &super::chunk::ChunkStats, value: &Value) -> bool {
    stats
        .min
        .as_ref()
        .zip(stats.max.as_ref())
        .is_some_and(|(min, max)| value < min || value > max)
}

/// Whether one decoded row satisfies the predicate test. A null slot never
/// satisfies a comparison with a non-null constant (the store's three-valued
/// logic decides real null semantics at the row authority; the scan only
/// narrows).
fn matches(test: &ColumnarTest, row: &Value) -> bool {
    if is_null(row) {
        return false;
    }
    match test {
        ColumnarTest::Compare { compare, value } => match compare {
            ColumnarCompare::Equal => row == value,
            ColumnarCompare::NotEqual => row != value,
            ColumnarCompare::Less => row < value,
            ColumnarCompare::LessOrEqual => row <= value,
            ColumnarCompare::Greater => row > value,
            ColumnarCompare::GreaterOrEqual => row >= value,
        },
        ColumnarTest::In { values } => values.iter().any(|value| !is_null(value) && row == value),
    }
}

/// Whether a value is one of the store's two absences.
fn is_null(value: &Value) -> bool {
    matches!(value, Value::Null | Value::None)
}
