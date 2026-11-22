//! Deterministic training for a partitioned vector index (ADR-0141 D1).
//!
//! Two k-means runs, both pure functions of the samples and a seed:
//!
//! - a **coarse** run that finds the partition centroids — `partitions` of
//!   them, under the index's own distance, because a partition is "the records
//!   nearest this centroid" and nearness is declared in the metric; and
//! - one run **per subspace** that finds the 256 codebook centroids a record's
//!   product-quantized codes name. Subspaces always train on euclidean
//!   distance: a code approximates a fragment of the vector, and the
//!   approximation error is measured component by component, whatever the
//!   index's metric does with the whole.
//!
//! # Why every input is fixed
//!
//! Index entries are derived from the log, so a replica must be able to
//! recompute this state from the same records and reach the same bytes — the
//! same contract the graph's one-layer choice serves. So: the seed is a hash
//! of the index's own address (nothing time-based, nothing process-based);
//! the iteration counts are constants; the samples are taken by a fixed
//! stride over the id-sorted rows, the same sampling pattern the recall
//! measurement uses; and empty clusters keep their previous centroid rather
//! than inviting a tie-break nobody specified.

use tessari_encoding::{IndexAddress, VectorTrained};
use tessari_types::RecordId;

use crate::catalog::VectorDistance;
use crate::graph::separation;

/// How wide one product-quantizer subspace is: one byte of code per four
/// components, so a 768-component vector codes to 192 bytes. Four rather than
/// the literature's usual eight because the measured corpus (release bench,
/// 20 000 × 32-dim) loses recall to coarser subspaces — eight components a
/// byte gave 78 % after rescoring where four gives the figure the bench
/// reports, at a footprint the 16× criterion still clears a dozen times over.
pub(crate) const SUBSPACE_WIDTH: usize = 4;

/// How many centroids one subspace's codebook holds — the whole range of one
/// code byte.
pub(crate) const PQ_CENTROIDS: usize = 256;

/// The most samples the coarse run trains over; beyond this the corpus is
/// stride-sampled down, which keeps the build cost a function of the
/// partitions rather than the table.
pub(crate) const TRAIN_SAMPLE_CAP: usize = 16384;

/// The most samples one subspace's codebook trains over. Codebook training
/// multiplies the cost by the subspace count, so it samples harder.
pub(crate) const PQ_SAMPLE_CAP: usize = 16384;

/// Fixed iteration counts — a constant, not a convergence test, because a
/// convergence test on float sums is a process that two machines can answer
/// differently at the last ulp.
pub(crate) const K_MEANS_ITERS: usize = 25;
pub(crate) const PQ_ITERS: usize = 8;

/// Train over these rows, or `None` when there is nothing to train on (the
/// callers write no trained state, exactly as an empty build writes no
/// graph). `rows` must be in record-id order — the callers build it from a
/// `BTreeMap`, which is — because the stride sample is a function of that
/// order.
#[must_use]
pub(crate) fn train(
    address: &IndexAddress,
    partitions: u32,
    distance: VectorDistance,
    rows: &[(RecordId, Vec<f64>)],
) -> Option<VectorTrained> {
    let dimensions = u32::try_from(rows.first().map_or(0, |(_, vector)| vector.len())).ok()?;
    if dimensions == 0 {
        return None;
    }
    let subspaces = dimensions.div_ceil(u32::try_from(SUBSPACE_WIDTH).unwrap_or(u32::MAX));
    let seed = seed_of(address);
    let held = stride_sample(rows, TRAIN_SAMPLE_CAP);
    let held: Vec<Vec<f64>> = held.iter().map(|(_, vector)| vector.clone()).collect();
    let coarse = k_means(
        &held,
        usize::try_from(partitions).unwrap_or(usize::MAX).max(1),
        usize::try_from(dimensions).unwrap_or(0),
        K_MEANS_ITERS,
        seed,
        distance,
        matches!(distance, VectorDistance::Cosine),
    );
    let width = usize::try_from(dimensions).unwrap_or(0);
    let sub_width = usize::try_from(subspaces).ok()?.max(1);
    let narrow = stride_sample(rows, PQ_SAMPLE_CAP);
    let codebook_size = PQ_CENTROIDS.min(narrow.len().max(1));
    let mut codebooks = Vec::with_capacity(
        sub_width
            .saturating_mul(codebook_size)
            .saturating_mul(SUBSPACE_WIDTH),
    );
    for subspace in 0..sub_width {
        let at = subspace.saturating_mul(SUBSPACE_WIDTH);
        let points: Vec<Vec<f64>> = narrow
            .iter()
            .map(|(_, vector)| {
                vector
                    .get(at..at.saturating_add(SUBSPACE_WIDTH).min(width))
                    .unwrap_or(&[])
                    .to_vec()
            })
            .collect();
        let actual_width = points.first().map_or(0, Vec::len);
        codebooks.extend(k_means(
            &points,
            codebook_size,
            actual_width,
            PQ_ITERS,
            seed ^ u64::try_from(subspace).unwrap_or(u64::MAX),
            VectorDistance::Euclidean,
            false,
        ));
    }
    Some(VectorTrained {
        partitions,
        dimensions,
        subspaces: u32::try_from(sub_width).unwrap_or(u32::MAX),
        codebook_size: u32::try_from(codebook_size).unwrap_or(u32::MAX),
        centroids: coarse,
        codebooks,
    })
}

/// Every `stride`-th row, at most `cap` — deterministic because the rows are
/// id-sorted, and biased toward spread rather than toward the first-written
/// records, for the reason the recall measurement gives.
fn stride_sample(rows: &[(RecordId, Vec<f64>)], cap: usize) -> Vec<(RecordId, Vec<f64>)> {
    if rows.len() <= cap {
        return rows.to_vec();
    }
    let stride = rows.len().div_ceil(cap);
    rows.iter().step_by(stride).cloned().collect()
}

/// The seed this index trains with: its own address, hashed. Two indexes over
/// the same table train differently; one index trains the same on every
/// replica.
#[must_use]
fn seed_of(address: &IndexAddress) -> u64 {
    u64::from(tessari_encoding::murmur2(
        &address.prefix(tessari_encoding::KeyKind::VectorTrained),
    ))
    .wrapping_mul(0x9E37_79B9_7F4A_7C15)
    .wrapping_add(1)
}

/// One k-means run over `points`, answering `k × width` flat components.
///
/// Centroids initialize from a seeded shuffle of the points (the seed's
/// load-bearing use: it breaks the correlation an ordered corpus would
/// otherwise hand a spaced pick), recompute as per-cluster means — normalized
/// when `normalize`, because a cosine centroid that kept its magnitude would
/// pull every assignment toward long vectors — and a cluster that lost every
/// point keeps its previous centre, so the run is total and deterministic.
/// Integer work goes through iterators and `checked_*`: the house lints deny
/// bare operators on indices, and the methods say the same thing without the
/// audit surface.
fn k_means(
    points: &[Vec<f64>],
    k: usize,
    width: usize,
    iters: usize,
    seed: u64,
    distance: VectorDistance,
    normalize: bool,
) -> Vec<f64> {
    let usable: Vec<&Vec<f64>> = points
        .iter()
        .filter(|vector| vector.len() == width && !vector.is_empty())
        .collect();
    if usable.is_empty() || k == 0 || width == 0 {
        return Vec::new();
    }
    let mut centroids = initial_centroids(&usable, k, seed, width);
    for _ in 0..iters.max(1) {
        let mut sums = vec![vec![0.0_f64; width]; k];
        let mut counts = vec![0_u32; k];
        for point in &usable {
            let nearest = nearest_centroid(&centroids, point, width, distance);
            for (sum, component) in sums[nearest].iter_mut().zip(point.iter()) {
                *sum += component;
            }
            counts[nearest] = counts[nearest].saturating_add(1);
        }
        for (cluster, (sum, count)) in centroids
            .chunks_mut(width)
            .zip(sums.iter().zip(counts.iter()))
        {
            if *count == 0 {
                continue;
            }
            let count = f64::from(*count);
            for (held, total) in cluster.iter_mut().zip(sum.iter()) {
                *held = total / count;
            }
            if normalize {
                let norm = cluster.iter().map(|held| held * held).sum::<f64>().sqrt();
                if norm > 0.0 {
                    for held in cluster.iter_mut() {
                        *held /= norm;
                    }
                }
            }
        }
    }
    centroids
}

/// The first `k` points of a seeded Fisher–Yates shuffle; cycling picks when
/// there are fewer points than centroids.
fn initial_centroids(points: &[&Vec<f64>], k: usize, seed: u64, width: usize) -> Vec<f64> {
    let mut order: Vec<usize> = (0..points.len()).collect();
    let mut random = SplitMix64::new(seed);
    for at in (1..order.len()).rev() {
        let span = at.saturating_add(1);
        let other = usize::try_from(random.next())
            .unwrap_or(0)
            .checked_rem(span)
            .unwrap_or(0)
            .min(at);
        order.swap(at, other);
    }
    let picks: Vec<&Vec<f64>> = order.iter().map(|&at| points[at]).collect();
    let mut held = Vec::with_capacity(k.saturating_mul(width));
    for point in picks.iter().cycle().take(k) {
        held.extend_from_slice(&point[..width]);
    }
    held
}

/// Which centroid is nearest, ties on the lowest index — the same tie rule
/// the graph walks by.
fn nearest_centroid(
    centroids: &[f64],
    point: &[f64],
    width: usize,
    distance: VectorDistance,
) -> usize {
    let mut best = 0;
    let mut best_distance = f64::INFINITY;
    for (which, centroid) in centroids.chunks_exact(width.max(1)).enumerate() {
        if centroid.len() != point.len() {
            continue;
        }
        let held = separation(distance, point, centroid);
        if held < best_distance {
            best = which;
            best_distance = held;
        }
    }
    best
}

/// The smallest deterministic PRNG that serves: splitmix64, no dependencies,
/// no shared state — the seed is hashed from the index identity and every
/// draw is replayable on every replica.
struct SplitMix64(u64);

impl SplitMix64 {
    #[must_use]
    const fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut held = self.0;
        held = (held ^ (held >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        held = (held ^ (held >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        held ^ (held >> 31)
    }
}
