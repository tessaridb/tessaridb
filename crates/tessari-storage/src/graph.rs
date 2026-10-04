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

mod building;
mod filtered;
#[cfg(test)]
mod lazily;
mod nodes;
mod searching;

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
