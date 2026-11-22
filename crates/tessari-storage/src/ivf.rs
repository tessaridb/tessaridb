//! The partitioned (IVF, product-quantized) vector index kind (ADR-0141).
//!
//! Beside the navigable graph, a vector index may instead be a set of
//! partitions over a product-quantized corpus. Where the graph answers by
//! walking, this kind answers by **probing**: rank the partition centroids by
//! the query's distance, read the probed partitions as prefix ranges, and
//! offer the records whose codes place them nearest. The codes choose who is
//! tried; the records' own full vectors decide the order — the same contract
//! the `QUANTIZED` graph kind states, one level down in memory.
//!
//! # What it is for
//!
//! Measured in the SG1 study (release 0.51.0): the full graph holds ~6.4 KB a
//! vector in RAM and its walk costs scan time at a one-per-cent filter
//! selectivity. This kind exists for both walls: codes are ~`dimensions / 8`
//! bytes a vector and sit in the index keyspace, and a filtered probe reads
//! only the partitions it probes, spending a distance only on a record the
//! predicate already admitted.
//!
//! # The derived-state contract
//!
//! Everything here — trained state, assignments, codes — is a pure function
//! of the rows and the index identity, so a replica replays one log and
//! derives byte-identical state (ADR-0141 D1, amended: the trained state
//! travels as index entries, like the graph's, not in the catalog record).
//! Write maintenance assigns and codes against the stored trained state; it
//! never re-trains.

pub(crate) mod training;

use tessari_encoding::{
    IndexAddress, StoreKey, StoreValue, VectorCodeKey, VectorCodes, VectorRecall, VectorRecallKey,
    VectorTrained, VectorTrainedKey,
};
use tessari_kv::{KvBackend, ScanDirection, ScanRequest, WriteBatch};
use tessari_types::RecordId;

use crate::catalog::VectorDistance;
use crate::error::Result;
use crate::graph::{self, Admit, Matched, filtered_ceiling, insert_sorted, separation};
use training::SUBSPACE_WIDTH;

/// How many partitions a probe reads when the statement names no `EFFORT` —
/// the same default budget the graph walks with.
pub(crate) const DEFAULT_PROBES: usize = graph::EXPLORATION;

/// What one probe is looking for: the query, how many records the answer
/// asks for, and the partition budget the read named, if it named one.
#[derive(Clone, Copy)]
pub(crate) struct Probe<'q> {
    /// The vector the read measures against.
    pub(crate) query: &'q [f64],
    /// How many records the answer asks for.
    pub(crate) wanted: usize,
    /// The partition budget the read named, if it named one.
    pub(crate) effort: Option<usize>,
}

/// Write this index's trained state into the batch.
pub(crate) fn write_trained(
    batch: WriteBatch,
    address: &IndexAddress,
    trained: &VectorTrained,
) -> WriteBatch {
    batch.put(
        VectorTrainedKey::keyspace(),
        VectorTrainedKey::new(*address).encode(),
        trained.encode(),
    )
}

/// Read this index's trained state from committed storage, if a build wrote
/// any.
///
/// # Errors
///
/// Returns an error when the backend fails or the stored value cannot be
/// decoded.
pub(crate) async fn read_trained(
    backend: &dyn KvBackend,
    address: &IndexAddress,
) -> Result<Option<VectorTrained>> {
    let key = VectorTrainedKey::new(*address).encode();
    Ok(
        match backend.get(VectorTrainedKey::keyspace(), &key).await? {
            Some(bytes) => Some(VectorTrained::decode(bytes.as_slice())?),
            None => None,
        },
    )
}

/// Which partition this vector belongs to: the nearest centroid, ties on the
/// lowest partition — the graph's tie rule, so assignment is a function of
/// the trained state and the vector alone.
#[must_use]
pub(crate) fn assign(trained: &VectorTrained, distance: VectorDistance, vector: &[f64]) -> u32 {
    let width = usize::try_from(trained.dimensions).unwrap_or(0);
    let mut best = 0_u32;
    let mut best_distance = f64::INFINITY;
    for partition in 0..trained.partitions {
        let centroid = trained.centroid(partition);
        if centroid.len() != width || vector.len() != width {
            return 0;
        }
        let held = separation(distance, vector, centroid);
        if held < best_distance {
            best = partition;
            best_distance = held;
        }
    }
    best
}

/// This vector's codes: per subspace, the nearest codebook centroid.
///
/// A vector of the wrong width codes to nothing — it is infinitely far from
/// everything, and coding it would store a claim about where it belongs that
/// its own components do not support.
#[must_use]
pub(crate) fn code(trained: &VectorTrained, vector: &[f64]) -> Option<VectorCodes> {
    let dimensions = usize::try_from(trained.dimensions).unwrap_or(0);
    if vector.len() != dimensions || dimensions == 0 {
        return None;
    }
    let subspaces = usize::try_from(trained.subspaces).unwrap_or(0);
    let width = trained.subspace_width();
    let mut codes = Vec::with_capacity(subspaces);
    for subspace in 0..subspaces {
        let at = subspace.saturating_mul(SUBSPACE_WIDTH);
        let sub = &vector[at..at.saturating_add(SUBSPACE_WIDTH).min(dimensions)];
        let book = trained.codebook(subspace);
        let mut best = 0_u8;
        let mut best_distance = f64::INFINITY;
        for candidate in 0..book.len().checked_div(width.max(1)).unwrap_or(0) {
            let Some(centroid) = book.get(
                candidate.saturating_mul(width)..candidate.saturating_add(1).saturating_mul(width),
            ) else {
                break;
            };
            let held = separation(VectorDistance::Euclidean, sub, centroid);
            if held < best_distance {
                best = u8::try_from(candidate).unwrap_or(u8::MAX);
                best_distance = held;
            }
        }
        codes.push(best);
    }
    Some(VectorCodes { codes })
}

/// How far these codes place the record from the query: decode each subspace
/// through its codebook centroid and measure the whole — one allocation a
/// candidate, the same shape of cost a graph node pays.
#[must_use]
pub(crate) fn code_distance(
    trained: &VectorTrained,
    distance: VectorDistance,
    codes: &[u8],
    query: &[f64],
) -> f64 {
    let dimensions = usize::try_from(trained.dimensions).unwrap_or(0);
    let subspaces = usize::try_from(trained.subspaces).unwrap_or(0);
    if codes.len() != subspaces || query.len() != dimensions {
        return f64::INFINITY;
    }
    let mut approx = Vec::with_capacity(dimensions);
    for (subspace, code) in codes.iter().enumerate().take(subspaces) {
        let at = subspace.saturating_mul(SUBSPACE_WIDTH);
        let width = trained.subspace_width().min(dimensions.saturating_sub(at));
        let book = trained.codebook(subspace);
        let from = usize::from(*code).saturating_mul(trained.subspace_width());
        let Some(centroid) = book.get(from..from.saturating_add(width)) else {
            approx.resize(at.saturating_add(width), 0.0);
            continue;
        };
        approx.extend_from_slice(centroid);
    }
    separation(distance, &approx, query)
}

/// The partitions a probe reads, nearest centroid first.
///
/// `EFFORT` names the probe count, clamped to `[wanted, partitions]` — a
/// probe that reads fewer partitions than the answer asks for records cannot
/// fill the answer, and a probe that reads more partitions than exist reads
/// the index twice.
#[must_use]
pub(crate) fn probe_order(
    trained: &VectorTrained,
    distance: VectorDistance,
    probe: Probe<'_>,
) -> Vec<u32> {
    let dimensions = usize::try_from(trained.dimensions).unwrap_or(0);
    let mut ranked: Vec<(f64, u32)> = (0..trained.partitions)
        .filter(|partition| trained.centroid(*partition).len() == dimensions)
        .map(|partition| {
            (
                separation(distance, probe.query, trained.centroid(partition)),
                partition,
            )
        })
        .collect();
    ranked.sort_by(|left, right| {
        left.0
            .total_cmp(&right.0)
            .then_with(|| left.1.cmp(&right.1))
    });
    let probes = probe
        .effort
        .unwrap_or(DEFAULT_PROBES)
        .max(probe.wanted)
        .min(ranked.len());
    ranked
        .into_iter()
        .take(probes)
        .map(|(_, held)| held)
        .collect()
}

/// Scan one partition's codes, calling `seen` for each.
///
/// # Errors
///
/// Returns an error when the backend fails or a stored code cannot be
/// decoded.
async fn scan_partition(
    backend: &dyn KvBackend,
    address: &IndexAddress,
    partition: u32,
    mut seen: impl FnMut(RecordId, &[u8]),
) -> Result<()> {
    let prefix = VectorCodeKey::partition_prefix(address, partition);
    let request = ScanRequest {
        keyspace: VectorCodeKey::keyspace(),
        range: tessari_kv::KeyRange::prefix(&prefix),
        direction: ScanDirection::Forward,
        limit: None,
    };
    for (key, value) in backend.scan(&request).await? {
        let held = VectorCodeKey::decode(key.as_slice())?;
        let codes = VectorCodes::decode(value.as_slice())?;
        seen(held.id, &codes.codes);
    }
    Ok(())
}

/// The records this index says are nearest the query, nearest first.
///
/// **Approximate**, exactly as the graph's walk is: the codes were lossy when
/// they were written, and an unprobed partition is a region the read agreed
/// to miss. See [`crate::VectorGraph::nearest`] for the contract the
/// statement's `APPROXIMATE` opts into.
///
/// # Errors
///
/// Returns an error when the backend fails or a stored code cannot be
/// decoded.
pub(crate) async fn nearest(
    backend: &dyn KvBackend,
    address: &IndexAddress,
    trained: &VectorTrained,
    distance: VectorDistance,
    probe: Probe<'_>,
) -> Result<Vec<RecordId>> {
    let mut best: Vec<(f64, RecordId)> = Vec::with_capacity(probe.wanted);
    for partition in probe_order(trained, distance, probe) {
        scan_partition(backend, address, partition, |id, codes| {
            insert_sorted(
                &mut best,
                code_distance(trained, distance, codes, probe.query),
                id,
                probe.wanted,
            );
        })
        .await?;
    }
    Ok(best
        .into_iter()
        .take(probe.wanted)
        .map(|(_, id)| id)
        .collect())
}

/// The admitted records nearest this vector, nearest first.
///
/// The filtered twin of [`nearest`], with the same contract as
/// [`crate::VectorGraph::nearest_matching`]: `admit` is asked at most once
/// per record and only for one that would enter the answer as it stands, and
/// the probe stops at the same ceiling — after which the caller answers
/// exactly, because a filtered read that came back short is not one the
/// statement's `APPROXIMATE` agreed to.
///
/// # Errors
///
/// Returns the first error `admit` returns, and a storage error when a stored
/// code cannot be decoded.
pub(crate) async fn nearest_matching<A>(
    backend: &dyn KvBackend,
    address: &IndexAddress,
    trained: &VectorTrained,
    distance: VectorDistance,
    probe: Probe<'_>,
    admit: &mut A,
) -> std::result::Result<Matched, A::Error>
where
    A: Admit,
    A::Error: From<crate::error::Error>,
{
    let ceiling = filtered_ceiling(probe.wanted, probe.effort);
    let mut best: Vec<(f64, RecordId)> = Vec::with_capacity(probe.wanted);
    let mut scanned = 0_usize;
    let mut cut = false;
    'partitions: for partition in probe_order(trained, distance, probe) {
        let mut check = Vec::new();
        scan_partition(backend, address, partition, |id, codes| {
            let held = code_distance(trained, distance, codes, probe.query);
            scanned = scanned.saturating_add(1);
            // Only a record that could enter the answer as it stands pays for
            // an admission: the graph's laziness rule, kept exactly.
            if best.len() < probe.wanted
                || held < best.last().map_or(f64::INFINITY, |(furthest, _)| *furthest)
            {
                check.push((held, id));
            }
        })
        .await
        .map_err(A::Error::from)?;
        for (held, id) in check {
            if best.len() >= probe.wanted && scanned >= ceiling {
                cut = true;
                break 'partitions;
            }
            if !admit.admit(&id).await? {
                continue;
            }
            insert_sorted(&mut best, held, id, probe.wanted);
        }
    }
    if best.len() < probe.wanted && scanned >= ceiling {
        cut = true;
    }
    Ok(Matched {
        ids: best.into_iter().map(|(_, id)| id).collect(),
        cut,
    })
}

/// A partitioned index's probe handle: the trained state and where the codes
/// live, independent of any transaction — the twin of what
/// [`crate::VectorGraph`] is to the graph, handed to a filtered read so the
/// caller can admit records against its own mutable transaction while the
/// probe scans.
pub struct Partitioned {
    engine: std::sync::Arc<dyn KvBackend>,
    address: IndexAddress,
    trained: VectorTrained,
    distance: VectorDistance,
}

impl Partitioned {
    pub(crate) fn new(
        engine: std::sync::Arc<dyn KvBackend>,
        address: IndexAddress,
        trained: VectorTrained,
        distance: VectorDistance,
    ) -> Self {
        Self {
            engine,
            address,
            trained,
            distance,
        }
    }

    /// The admitted records nearest this vector, nearest first — the same
    /// contract as [`crate::VectorGraph::nearest_matching`], including the
    /// ceiling: `cut` says the probe stopped at its budget rather than out of
    /// candidates, after which the caller answers exactly.
    ///
    /// # Errors
    ///
    /// Returns the first error `admit` returns, and a storage error when a
    /// stored code cannot be decoded.
    pub async fn nearest_matching<A>(
        &self,
        query: &[f64],
        wanted: usize,
        effort: Option<usize>,
        admit: &mut A,
    ) -> std::result::Result<Matched, A::Error>
    where
        A: Admit,
        A::Error: From<crate::error::Error>,
    {
        nearest_matching(
            self.engine.as_ref(),
            &self.address,
            &self.trained,
            self.distance,
            Probe {
                query,
                wanted,
                effort,
            },
            admit,
        )
        .await
    }
}

/// Measure this index's recall over the rows it was built from, and write the
/// figure beside the entries — the same honest pattern the graph's build
/// uses: probes are compared against the exact answer, the queries are the
/// store's own vectors sampled by a fixed stride over record-id order, and
/// the query record is excluded from both sides of the comparison.
///
/// The probe runs over the rows **in hand**, like the graph's measurement —
/// the codes this build just computed, not the stored keyspace, which this
/// batch has not landed in yet. (Scanning the backend here would measure the
/// previous build, or nothing at all on a first one.)
///
/// `None` when there is nothing to measure — an index over fewer than two
/// records has no answer a probe could get wrong, and absence reads as
/// *never measured*, which is a different statement from a measured zero.
///
/// # Errors
///
/// Returns an error when a row cannot be coded (which a build against these
/// rows does not produce).
pub(crate) async fn measure(
    batch: WriteBatch,
    address: &IndexAddress,
    trained: &VectorTrained,
    distance: VectorDistance,
    rows: &[(RecordId, Vec<f64>)],
) -> Result<WriteBatch> {
    let records = rows.len();
    if records < 2 {
        return Ok(batch);
    }
    let coded: Vec<(RecordId, Vec<u8>, u32)> = rows
        .iter()
        .filter_map(|(id, vector)| {
            code(trained, vector)
                .map(|held| (id.clone(), held.codes, assign(trained, distance, vector)))
        })
        .collect();
    let stride = records.div_ceil(graph::MEASURED_SAMPLE).max(1);
    let mut hit = 0_usize;
    let mut asked = 0_usize;
    let mut sample = 0_usize;
    for (id, vector) in rows.iter().step_by(stride) {
        let mut truth: Vec<(f64, RecordId)> = Vec::with_capacity(graph::MEASURED_AT);
        for (other, other_vector) in rows {
            if other == id {
                continue;
            }
            insert_sorted(
                &mut truth,
                separation(distance, other_vector, vector),
                other.clone(),
                graph::MEASURED_AT,
            );
        }
        if truth.is_empty() {
            continue;
        }
        // The probe, over the rows in hand: the nearest centroids' partitions,
        // the codes ranked by their distance, the answer trimmed to what a
        // read would ask.
        let probes = probe_order(
            trained,
            distance,
            Probe {
                query: vector,
                wanted: graph::MEASURED_AT.saturating_add(1),
                effort: Some(measured_probes(trained.partitions)),
            },
        );
        let mut found: Vec<(f64, RecordId)> = Vec::with_capacity(graph::MEASURED_AT);
        for (other, codes, partition) in &coded {
            if !probes.contains(partition) {
                continue;
            }
            insert_sorted(
                &mut found,
                code_distance(trained, distance, codes, vector),
                other.clone(),
                graph::MEASURED_AT.saturating_add(1),
            );
        }
        let found: Vec<RecordId> = found
            .into_iter()
            .map(|(_, other)| other)
            .filter(|other| other != id)
            .take(graph::MEASURED_AT)
            .collect();
        hit = hit.saturating_add(
            found
                .iter()
                .filter(|got| truth.iter().any(|(_, truth_id)| truth_id == *got))
                .count(),
        );
        asked = asked.saturating_add(truth.len());
        sample = sample.saturating_add(1);
    }
    let Some(recall) = hit.saturating_mul(100).checked_div(asked) else {
        return Ok(batch);
    };
    Ok(batch.put(
        VectorRecallKey::keyspace(),
        VectorRecallKey::new(*address).encode(),
        VectorRecall {
            recall: u32::try_from(recall).unwrap_or(100),
            at: u32::try_from(graph::MEASURED_AT).unwrap_or(u32::MAX),
            sample: u32::try_from(sample).unwrap_or(u32::MAX),
            records: u64::try_from(records).unwrap_or(u64::MAX),
            neighbours: trained.partitions,
            exploration: u32::try_from(measured_probes(trained.partitions)).unwrap_or(u32::MAX),
        }
        .encode(),
    ))
}

/// The probe count a measurement ran at: the default budget, capped at the
/// partition count — probing more partitions than exist reads the index
/// twice.
#[must_use]
fn measured_probes(partitions: u32) -> usize {
    DEFAULT_PROBES.min(usize::try_from(partitions).unwrap_or(usize::MAX))
}

#[cfg(test)]
mod tests;
