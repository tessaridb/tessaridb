//! The pruning scan (SG3 T3.2): chunks examined against their head
//! statistics, survivors decoded, the predicate re-evaluated per row.
//!
//! # Filter-and-refine, never believe (worksheet §6.1, OLAP skill M3)
//!
//! A chunk the statistics say cannot match is **skipped**; every other chunk
//! is decoded and its rows are tested against the real predicate. The hits
//! are **candidates, not results**: a tombstoned record's row still sits in
//! its chunk (D-SG2-9 — delete writes no chunk rewrite), a bloom says
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

/// One family's predicate: the projection of `family`, compared to a
/// constant the caller evaluated and bound.
#[derive(Clone, Debug)]
pub struct ColumnarPredicate {
    /// The projected family under test.
    pub family: ColumnFamily,
    /// The comparison.
    pub compare: ColumnarCompare,
    /// The constant, evaluated and bound by the caller. A null constant
    /// never prunes and never matches here; the row authority above
    /// decides null semantics for real.
    pub value: Value,
}

/// What the scan did — the worksheet §6.2 counters the answer reports.
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

/// One family's chunks: prune by the head statistics, decode survivors,
/// test each row.
async fn scan_family(
    store: &Store,
    namespace: NamespaceId,
    database: DatabaseId,
    table: TableId,
    predicate: &ColumnarPredicate,
    prune: bool,
    report: &mut ColumnarScanReport,
) -> Result<BTreeSet<RecordId>> {
    let open = match newest_under(
        store,
        &PointerKey::versions_prefix(namespace, database, table, &predicate.family),
    )
    .await?
    {
        Some((_, bytes)) => super::project::seq_of(&bytes),
        None => return Ok(BTreeSet::new()),
    };
    let mut hits = BTreeSet::new();
    let mut seq = 0_u64;
    while seq <= open {
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
            if prune && pruned_by_head(predicate, &bytes)? {
                report.chunks_skipped = report.chunks_skipped.saturating_add(1);
            } else {
                let chunk = decode_chunk(&bytes)?;
                report.rows_decoded = report
                    .rows_decoded
                    .saturating_add(u64::try_from(chunk.values.len()).unwrap_or(u64::MAX));
                for (id, value) in chunk.record_ids.iter().zip(chunk.values.iter()) {
                    if matches(&predicate.compare, &predicate.value, value) {
                        hits.insert(id.clone());
                    }
                }
            }
        }
        seq = seq.saturating_add(1);
    }
    Ok(hits)
}

/// Whether the head statistics prove no row of this chunk can match.
fn pruned_by_head(predicate: &ColumnarPredicate, bytes: &[u8]) -> Result<bool> {
    let (_, stats) = decode_head(bytes)?;
    if is_null(&predicate.value) {
        return Ok(false);
    }
    let value = &predicate.value;
    match &predicate.compare {
        ColumnarCompare::NotEqual => Ok(false),
        ColumnarCompare::Equal => {
            if let Some(bloom) = &stats.bloom {
                return Ok(!bloom.might_contain(&encode_payload(value).into_bytes()));
            }
            Ok(outside(&stats, value))
        }
        ColumnarCompare::Less => Ok(stats.min.as_ref().is_some_and(|min| min >= value)),
        ColumnarCompare::LessOrEqual => Ok(stats.min.as_ref().is_some_and(|min| min > value)),
        ColumnarCompare::Greater => Ok(stats.max.as_ref().is_some_and(|max| max <= value)),
        ColumnarCompare::GreaterOrEqual => Ok(stats.max.as_ref().is_some_and(|max| max < value)),
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

/// Whether one decoded row satisfies the predicate. A null slot never
/// satisfies a comparison with a non-null constant (the store's three-valued
/// logic decides real null semantics at the row authority; the scan only
/// narrows).
fn matches(compare: &ColumnarCompare, constant: &Value, row: &Value) -> bool {
    if is_null(row) {
        return false;
    }
    match compare {
        ColumnarCompare::Equal => row == constant,
        ColumnarCompare::NotEqual => row != constant,
        ColumnarCompare::Less => row < constant,
        ColumnarCompare::LessOrEqual => row <= constant,
        ColumnarCompare::Greater => row > constant,
        ColumnarCompare::GreaterOrEqual => row >= constant,
    }
}

/// Whether a value is one of the store's two absences.
fn is_null(value: &Value) -> bool {
    matches!(value, Value::Null | Value::None)
}
