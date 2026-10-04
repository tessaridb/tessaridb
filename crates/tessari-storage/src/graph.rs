//! The navigable graph a vector index is.
//!
//! # Why a graph, and why only one layer
//!
//! A linear nearest-neighbour read computes a distance per record. Nothing can
//! make one distance much cheaper — measured on this store it is about 1.8 µs
//! over two thousand records, dominated by walking values rather than by the
//! arithmetic — so the only saving available is to compute **fewer**. A
//! navigable small-world graph does that: a greedy walk visits a few dozen nodes
//! where a scan visits every one.
//!
//! The hierarchical form assigns each node a random level, and the layers
//! improve routing at large collection sizes. **A random level is exactly what
//! this store cannot have.** Index entries are derived from the log rather than
//! carried in it, so a replica must compute the same graph from the same
//! records; a level drawn from a generator makes two replicas disagree about
//! which ten records are nearest, and disagree silently, because a slightly
//! different neighbour list looks exactly like a right one.
//!
//! One layer needs no levels. Insertion order is log order, which every replica
//! replays identically, and every choice below breaks ties on the record id — so
//! the graph is a function of the records and their order, and nothing else. The
//! key still carries a level byte, reserved: a hash-derived level is the right
//! shape when the layers are worth their cost, and reserving room cannot be done
//! retroactively.
//!
//! # What it can and cannot promise
//!
//! It is **approximate**. A greedy walk returns the neighbours it found and
//! there is no way to show it missed none without the scan it exists to avoid.
//! That is why the language requires a statement to say `APPROXIMATE` before
//! this index may serve it — see `docs/tessariql.md`. Silence gets the scan.
//!
//! # Deletion, and where the decay lives
//!
//! Removing a record removes its node and the edges out of it. The edges **into**
//! it are left, because finding them means reading every node that might point
//! here. Correctness is unaffected: an index read is a candidate set and each
//! candidate is resolved at the reader's own snapshot, so a removed record never
//! reaches an answer. Recall is affected, and decays with churn — measured here
//! at **100% falling to 42%** when half of two thousand records go.
//!
//! The remedy is `REBUILD INDEX`, and it is a statement rather than something
//! this graph decides for itself. A store that rebuilt on its own reckoning
//! would rebuild on each replica at a different moment, and from that moment two
//! replicas would answer the same approximate question differently — the same
//! failure random levels would cause, arriving by a different road. Written as a
//! statement, the rebuild is a record in the log that every replica applies at
//! one sequence, and the rebuilt graph is a function of the rows alone because
//! `index::build` inserts them in record-id order.

use std::collections::{BTreeMap, BTreeSet};

use tessari_encoding::{
    IndexAddress, QuantizedVector, StoreKey, StoreValue, StoredVector, VectorNode, VectorNodeKey,
    VectorRecall, VectorRecallKey,
};
use tessari_kv::WriteBatch;
use tessari_types::{Number, RecordId, Value};

use crate::catalog::VectorDistance;
use crate::error::Result;
use crate::store::Store;

mod filtered;
#[cfg(test)]
mod lazily;
mod nodes;

use nodes::Nodes;

pub use filtered::{Matched, filtered_ceiling};

/// How many neighbours a node keeps.
///
/// The graph's only real tuning knob. Too few and the walk gets stuck in a local
/// pocket; too many and every insert rewrites a crowd of nodes. Sixteen is the
/// value the literature settles on for moderate dimensions, and it is a starting
/// value here for the same reason `k1` and `b` are: nothing in this project has
/// measured a recall curve to justify another.
pub(crate) const NEIGHBOURS: usize = 16;

/// How many candidates a walk keeps in hand.
///
/// Larger explores more and costs more, in a trade that is the whole of this
/// index's speed-against-recall dial. Sixty-four is a starting value, and the
/// harness measures what recall it buys rather than assuming one.
pub(crate) const EXPLORATION: usize = 64;

/// The single layer this graph has.
pub(crate) const GROUND: u8 = 0;

/// How many neighbours a measuring query asks for.
///
/// Ten, because that is the size of answer a caller actually asks a vector store
/// for, and a recall figure describes the answers people take rather than an
/// abstract one. It is reported beside the figure, since recall@1 and recall@10
/// are different numbers and a percentage that did not say which is unreadable.
const MEASURED_AT: usize = 10;

/// How many queries a measurement averages over.
///
/// The cost is `sample × records` distances against a build that already costs
/// roughly `records × EXPLORATION`, so thirty-two is a fraction of the statement
/// it rides on rather than a new expense. Small enough to be free, large enough
/// that one unlucky query cannot carry the figure.
const MEASURED_SAMPLE: usize = 32;

/// The vector a value holds, if it holds one.
///
/// Public because the session has to read a query vector the same way the index
/// read the stored ones; two readings of "is this a vector" would eventually
/// disagree about a record, and the disagreement would be a record in the graph
/// that no query can reach.
///
/// The same reading the distance functions do, and deliberately so: a record the
/// index accepts and the distance refuses — or the reverse — would be a record
/// that is in the graph and cannot be compared, or one that could be compared
/// and is not there.
#[must_use]
pub fn vector_of(value: &Value) -> Option<Vec<f64>> {
    let Value::Array(items) = value else {
        return None;
    };
    let held: Option<Vec<f64>> = items
        .iter()
        .map(|item| match item {
            Value::Number(Number::Float(component)) => Some(*component),
            Value::Number(other) => other
                .as_decimal()
                .and_then(|exact| f64::try_from(exact).ok()),
            _ => None,
        })
        .collect();
    held.filter(|components| !components.is_empty())
}

/// How far apart two vectors are, under the distance the index declared.
///
/// **The index's distance and no other.** A graph whose edges were chosen by one
/// measure approximates that measure: cosine ranks by angle and euclidean by
/// separation, and for vectors nobody normalised the two disagree. Serving a
/// cosine query from a euclidean graph would return plausible neighbours that
/// are not the nearest, which is the failure this node exists to avoid.
///
/// Euclidean answers the **squared** distance, because the graph only ever
/// compares — the square root is work whose result is thrown away, and omitting
/// it is not an approximation, since a square root is monotone over non-negative
/// numbers.
///
/// Mismatched or empty vectors answer `+∞`, the same "infinitely far" the
/// language's own distances give, so a vector of the wrong shape can never be a
/// neighbour.
fn separation(distance: VectorDistance, left: &[f64], right: &[f64]) -> f64 {
    if left.len() != right.len() || left.is_empty() {
        return f64::INFINITY;
    }
    match distance {
        VectorDistance::Euclidean => left
            .iter()
            .zip(right.iter())
            .map(|(a, b)| (a - b) * (a - b))
            .sum(),
        VectorDistance::Cosine => {
            let dot: f64 = left.iter().zip(right.iter()).map(|(a, b)| a * b).sum();
            let magnitude = norm(left) * norm(right);
            if magnitude == 0.0 {
                // A zero vector points nowhere, so it has no angle — the same
                // answer `vector::cosine` gives.
                return f64::INFINITY;
            }
            1.0 - dot / magnitude
        }
    }
}

fn norm(vector: &[f64]) -> f64 {
    vector.iter().map(|held| held * held).sum::<f64>().sqrt()
}

/// How far a stored vector is from a query, under the index's distance.
///
/// The same measure as [`separation`], read straight off the codes when the
/// vector is quantized — decoding each component as it is used rather than
/// building the full vector first, so a quantized node costs a walk one byte per
/// component instead of eight.
fn separation_from(distance: VectorDistance, stored: &StoredVector, query: &[f64]) -> f64 {
    let coded = match stored {
        StoredVector::Full(vector) => return separation(distance, vector, query),
        StoredVector::Quantized(coded) => coded,
    };
    if coded.codes.len() != query.len() || query.is_empty() {
        return f64::INFINITY;
    }
    let components = coded.codes.iter().map(|code| coded.component(*code));
    match distance {
        VectorDistance::Euclidean => components
            .zip(query.iter())
            .map(|(a, b)| (a - b) * (a - b))
            .sum(),
        VectorDistance::Cosine => {
            let (mut dot, mut own) = (-0.0, -0.0);
            for (a, b) in components.zip(query.iter()) {
                dot += a * b;
                own += a * a;
            }
            let magnitude = own.sqrt() * norm(query);
            if magnitude == 0.0 {
                return f64::INFINITY;
            }
            1.0 - dot / magnitude
        }
    }
}

/// The graph of one index, read from the committed state.
///
/// Read **node by node** as a walk reaches them, and held for the length of one
/// operation (G058 C2, Q-906): a walk reads the few hundred nodes it visits rather
/// than decoding the whole graph first. See [`nodes`] for the cache's key, bound
/// and invalidation. Only a recall measurement reads every node.
#[derive(Debug)]
pub struct Graph {
    nodes: Nodes,
    distance: VectorDistance,
    /// Whether a node placed in this graph keeps its vector as codes.
    quantized: bool,
}

impl Graph {
    /// An empty graph for this distance.
    pub(crate) fn empty(distance: VectorDistance, quantized: bool) -> Self {
        Self {
            nodes: Nodes::in_memory(),
            distance,
            quantized,
        }
    }

    /// The graph of an index, its nodes read as they are reached.
    pub(crate) fn read(
        store: &Store,
        address: &IndexAddress,
        distance: VectorDistance,
        quantized: bool,
    ) -> Result<Self> {
        Ok(Self {
            nodes: Nodes::stored(std::sync::Arc::clone(store.backend()), *address),
            distance,
            quantized,
        })
    }

    /// Whether the graph holds nothing.
    ///
    /// # Errors
    ///
    /// Returns an error when a stored node cannot be read.
    pub(crate) fn is_empty(&self) -> Result<bool> {
        Ok(self.entry()?.is_none())
    }

    /// The records nearest this vector, nearest first, at most `wanted` of them.
    ///
    /// A greedy walk from the entry point, keeping the best `effort` candidates
    /// seen. Approximate by construction — see the module documentation for why
    /// that is a language-level decision and not a detail.
    ///
    /// `effort` is `None` for the budget this engine was built with
    /// ([`EXPLORATION`]) and `Some` for a budget the **read** named. It is raised
    /// to at least `wanted`, because a walk that keeps fewer candidates than the
    /// answer asks for cannot fill the answer, and a budget silently overriding a
    /// `LIMIT` is a bound answering for a bound.
    ///
    /// **A read's budget never reaches the build.** [`Self::insert`] walks this
    /// same graph to choose a new node's neighbours and passes `None` — see the
    /// note there for what a leak would cost.
    pub(crate) fn nearest(
        &self,
        query: &[f64],
        wanted: usize,
        effort: Option<usize>,
    ) -> Result<Vec<RecordId>> {
        let effort = effort.unwrap_or(EXPLORATION).max(wanted);
        let Some(entry) = self.entry()? else {
            return Ok(Vec::new());
        };
        let mut seen: BTreeSet<RecordId> = BTreeSet::new();
        // Candidates to expand, and the best found so far. Both are kept sorted
        // by distance with the record id breaking ties, so the walk is a
        // function of the graph and the query and of nothing else.
        let mut frontier: Vec<(f64, RecordId)> = Vec::new();
        let mut best: Vec<(f64, RecordId)> = Vec::new();

        let start = self.at(entry.clone(), query)?;
        seen.insert(entry.clone());
        frontier.push((start, entry.clone()));
        best.push((start, entry));

        while let Some((_, current)) = take_nearest(&mut frontier) {
            let Some(node) = self.nodes.get(&current)? else {
                continue;
            };
            // Stop when nothing in hand can improve on what is already held:
            // the classic greedy cut-off, and what keeps the walk sub-linear.
            if let Some((furthest, _)) = best.last()
                && best.len() >= effort
                && self.at(current.clone(), query)? > *furthest
            {
                break;
            }
            for neighbour in &node.neighbours {
                if !seen.insert(neighbour.clone()) {
                    continue;
                }
                // An edge into a removed record is dangling — this graph does
                // not chase inbound edges when a node goes, so they exist. It is
                // not a candidate: offering it would answer with a record that
                // is not in the index, which the resolution step would drop and
                // which would meanwhile have taken a place in the answer.
                if !self.nodes.contains(neighbour)? {
                    continue;
                }
                let distance = self.at(neighbour.clone(), query)?;
                frontier.push((distance, neighbour.clone()));
                insert_sorted(&mut best, distance, neighbour.clone(), effort);
            }
        }

        Ok(best.into_iter().take(wanted).map(|(_, id)| id).collect())
    }

    /// What fraction of the true nearest this graph actually returns.
    ///
    /// The walk is compared against the exact answer over the same records, and
    /// the result is a **measurement** — the one thing [`VectorRecall`] is
    /// allowed to hold, and the reason it is not computed from [`NEIGHBOURS`]
    /// and [`EXPLORATION`] instead.
    ///
    /// # The queries are the store's own vectors, and that has a trap in it
    ///
    /// There are no others: nothing here records what anyone has searched for.
    /// So the sample is taken from the stored vectors themselves, **by position
    /// in key order** — every `⌈records / sample⌉`-th — which makes it a
    /// function of the stored set rather than of a draw, an insertion order or a
    /// clock. That matters for the same reason the graph has one layer: two
    /// replicas replaying one log must reach the same number.
    ///
    /// A stored vector queried against itself finds itself at **distance zero**.
    /// That is a free hit, and a measurement that kept it would report a floor of
    /// `1/at` on an index that finds nothing else — a figure that looks like a
    /// measurement and is not. So the query record is removed from both the
    /// truth and the answer, and the comparison is over what is left.
    ///
    /// Perturbing the sampled vectors instead was considered and rejected: a
    /// perturbation needs a random direction, and randomness is precisely what
    /// this index gave up its hierarchical layer to avoid.
    ///
    /// `None` when there is nothing to measure — an index over fewer than two
    /// records has no answer a walk could get wrong, and absence reads as *never
    /// measured*, which is a different statement from a measured zero.
    pub(crate) fn recall(&self) -> Result<Option<VectorRecall>> {
        self.nodes.load_all()?;
        let present = self.nodes.present();
        let records = present.len();
        if records < 2 {
            return Ok(None);
        }
        let stride = records.div_ceil(MEASURED_SAMPLE).max(1);
        let mut hit = 0_usize;
        let mut asked = 0_usize;
        let mut sample = 0_usize;
        for (id, node) in present.iter().step_by(stride) {
            let probe = node.vector.to_vec();
            let truth = exact(self.distance, &present, &probe, id, MEASURED_AT);
            if truth.is_empty() {
                continue;
            }
            // One more than the answer, because the query record is expected
            // back and is then dropped; `take` trims the case where it was not.
            let found: Vec<RecordId> = self
                .nearest(&probe, MEASURED_AT.saturating_add(1), None)?
                .into_iter()
                .filter(|other| other != id)
                .take(MEASURED_AT)
                .collect();
            hit = hit.saturating_add(found.iter().filter(|got| truth.contains(got)).count());
            asked = asked.saturating_add(truth.len());
            sample = sample.saturating_add(1);
        }
        let Some(recall) = hit.saturating_mul(100).checked_div(asked) else {
            return Ok(None);
        };
        Ok(Some(VectorRecall {
            recall: u32::try_from(recall).unwrap_or(100),
            at: u32::try_from(MEASURED_AT).unwrap_or(u32::MAX),
            sample: u32::try_from(sample).unwrap_or(u32::MAX),
            records: u64::try_from(records).unwrap_or(u64::MAX),
            neighbours: u32::try_from(NEIGHBOURS).unwrap_or(u32::MAX),
            exploration: u32::try_from(EXPLORATION).unwrap_or(u32::MAX),
        }))
    }

    /// How far this record is from the query, or infinitely far if it is gone.
    fn at(&self, id: RecordId, query: &[f64]) -> Result<f64> {
        Ok(self.nodes.get(&id)?.map_or(f64::INFINITY, |node| {
            separation_from(self.distance, &node.vector, query)
        }))
    }

    /// Where a walk starts.
    ///
    /// The smallest record id, so the entry point is a property of the data
    /// rather than of insertion order — which means it needs no key of its own
    /// and cannot drift out of step with the nodes.
    fn entry(&self) -> Result<Option<RecordId>> {
        self.nodes.first()
    }

    /// Place a record in the graph, and return every node the placement changed.
    ///
    /// The new node links to its nearest neighbours, and each of those gains the
    /// reverse edge — pruned back to [`NEIGHBOURS`] by distance, ties on record
    /// id. Without the reverse edge a new record is reachable from nowhere and
    /// the graph is a collection of one-way streets.
    pub(crate) fn insert(
        &mut self,
        id: &RecordId,
        vector: Vec<f64>,
    ) -> Result<BTreeMap<RecordId, VectorNode>> {
        let mut touched = BTreeMap::new();
        let chosen = if self.is_empty()? {
            Vec::new()
        } else {
            // `None`, and never a caller's budget. The build walks the graph to
            // choose this node's neighbours, so a read's `EFFORT` reaching here
            // would make the index a function of the reads that happened to run
            // beside the writes — and two replicas replaying one log would build
            // different graphs. That is the determinism this index gave up its
            // hierarchical layer to keep.
            self.nearest(&vector, NEIGHBOURS, None)?
        };
        let node = VectorNode::new(self.stored(vector), chosen.clone());
        self.nodes.put(id.clone(), node.clone());
        touched.insert(id.clone(), node);

        for neighbour in chosen {
            let Some(held) = self.nodes.get(&neighbour)? else {
                continue;
            };
            if held.neighbours.contains(id) {
                continue;
            }
            let mut linked = held.neighbours.clone();
            linked.push(id.clone());
            let pruned = self.prune(&held.vector.to_vec(), linked)?;
            let updated = VectorNode::new(held.vector.clone(), pruned);
            self.nodes.put(neighbour.clone(), updated.clone());
            touched.insert(neighbour, updated);
        }
        Ok(touched)
    }

    /// Keep [`NEIGHBOURS`] of these, chosen for **reach** rather than nearness.
    ///
    /// # Why not simply the nearest
    ///
    /// Because that is what stops the graph working, and it does so invisibly.
    /// Keeping the sixteen nearest makes every node's links point at its own
    /// immediate crowd, so a walk that starts in one region can never leave it —
    /// the long edges that make a small world small are exactly the ones a
    /// nearest-first rule throws away first. Measured on this store, nearest-M
    /// pruning gave **one per cent** of the true ten; the rule below gives most
    /// of them, on the same data, with the same walk.
    ///
    /// # The rule
    ///
    /// Walking the candidates nearest-first, a candidate is kept only if it is
    /// closer to the base than to anything already kept. A candidate that sits
    /// behind an existing neighbour is reachable **through** it and adds no new
    /// direction; one that opens a direction nothing else covers is kept however
    /// far away it is. So the neighbour list spans the space around a node
    /// instead of huddling on one side of it.
    ///
    /// Ties break on record id, so the choice is a function of the vectors and
    /// nothing else — which is what lets two replicas build one graph.
    fn prune(&self, from: &[f64], candidates: Vec<RecordId>) -> Result<Vec<RecordId>> {
        let mut ranked: Vec<(f64, RecordId)> = Vec::with_capacity(candidates.len());
        for id in candidates {
            ranked.push((self.at(id.clone(), from)?, id));
        }
        ranked.sort_by(|left, right| {
            left.0
                .total_cmp(&right.0)
                .then_with(|| left.1.cmp(&right.1))
        });
        ranked.dedup_by(|left, right| left.1 == right.1);

        let mut kept: Vec<(RecordId, Vec<f64>)> = Vec::new();
        for (to_base, id) in ranked {
            if kept.len() >= NEIGHBOURS {
                break;
            }
            let Some(held) = self.nodes.get(&id)? else {
                continue;
            };
            let covered = kept
                .iter()
                .any(|(_, other)| separation_from(self.distance, &held.vector, other) < to_base);
            if covered {
                continue;
            }
            kept.push((id, held.vector.to_vec()));
        }
        Ok(kept.into_iter().map(|(id, _)| id).collect())
    }

    /// The form a vector placed in this graph is kept in.
    ///
    /// Codes in a quantized graph, unless the vector cannot be coded (a component
    /// that is not finite), in which case it is kept whole rather than refused —
    /// the distance functions already place such a vector infinitely far away.
    fn stored(&self, vector: Vec<f64>) -> StoredVector {
        if self.quantized
            && let Some(coded) = QuantizedVector::of(&vector)
        {
            return StoredVector::Quantized(coded);
        }
        StoredVector::Full(vector)
    }

    /// Take a record out of the graph.
    pub(crate) fn remove(&mut self, id: &RecordId) {
        self.nodes.remove(id);
    }

    /// How many edges point at records this graph no longer holds.
    ///
    /// The measure of what churn has done. Public to the crate because the
    /// thing worth testing about a rebuild is not that it ran — it is that the
    /// dangling edges are gone and the recall came back.
    #[cfg(test)]
    pub(crate) fn dangling(&self) -> usize {
        if self.nodes.load_all().is_err() {
            return usize::MAX;
        }
        let present = self.nodes.present();
        let ids: BTreeSet<&RecordId> = present.iter().map(|(id, _)| id).collect();
        present
            .iter()
            .flat_map(|(_, node)| node.neighbours.iter())
            .filter(|id| !ids.contains(id))
            .count()
    }
}

/// The records genuinely nearest this vector among `present`, by looking at
/// every one.
///
/// The truth half of [`Graph::recall`], and `O(records)` per call by definition —
/// there is no cheaper way to know what a walk missed. `excluding` is the query's
/// own record, because a vector is always nearest to itself.
fn exact(
    distance: VectorDistance,
    present: &[(RecordId, std::sync::Arc<VectorNode>)],
    query: &[f64],
    excluding: &RecordId,
    wanted: usize,
) -> Vec<RecordId> {
    let mut held: Vec<(f64, RecordId)> = Vec::new();
    for (id, node) in present {
        if id == excluding {
            continue;
        }
        let separation = separation_from(distance, &node.vector, query);
        insert_sorted(&mut held, separation, id.clone(), wanted);
    }
    held.into_iter().map(|(_, id)| id).collect()
}

/// The nearest candidate, removed from the list.
fn take_nearest(frontier: &mut Vec<(f64, RecordId)>) -> Option<(f64, RecordId)> {
    let mut best = 0;
    for (position, held) in frontier.iter().enumerate() {
        let current = frontier.get(best)?;
        if held.0 < current.0 || (held.0 == current.0 && held.1 < current.1) {
            best = position;
        }
    }
    if frontier.is_empty() {
        return None;
    }
    Some(frontier.swap_remove(best))
}

/// Add a candidate to a sorted list, keeping it at most `cap` long.
fn insert_sorted(held: &mut Vec<(f64, RecordId)>, distance: f64, id: RecordId, cap: usize) {
    let at = held
        .binary_search_by(|(theirs, other)| {
            theirs.total_cmp(&distance).then_with(|| other.cmp(&id))
        })
        .unwrap_or_else(|position| position);
    held.insert(at, (distance, id));
    held.truncate(cap);
}

/// Write these nodes into the batch.
pub(crate) fn write(
    mut batch: WriteBatch,
    address: &IndexAddress,
    nodes: &BTreeMap<RecordId, VectorNode>,
) -> WriteBatch {
    for (id, node) in nodes {
        batch = batch.put(
            VectorNodeKey::keyspace(),
            VectorNodeKey::new(*address, GROUND, id.clone()).encode(),
            node.encode(),
        );
    }
    batch
}

/// Write this graph's measured recall into the batch, if it has one.
///
/// Called from a build, where the graph and every stored vector are already in
/// hand, so the measurement costs distances and no reads. A build clears the
/// index's keyspace first, so a graph with nothing to measure leaves **no** key
/// rather than a stale one — and absence is what `INFO FOR VECTOR` reports as
/// never measured.
pub(crate) fn measure(
    batch: WriteBatch,
    address: &IndexAddress,
    graph: &Graph,
) -> Result<WriteBatch> {
    let Some(measured) = graph.recall()? else {
        return Ok(batch);
    };
    Ok(batch.put(
        VectorRecallKey::keyspace(),
        VectorRecallKey::new(*address).encode(),
        measured.encode(),
    ))
}

/// Remove one node from the batch.
pub(crate) fn erase(batch: WriteBatch, address: &IndexAddress, id: &RecordId) -> WriteBatch {
    batch.delete(
        VectorNodeKey::keyspace(),
        VectorNodeKey::new(*address, GROUND, id.clone()).encode(),
    )
}

#[cfg(test)]
mod tests;
