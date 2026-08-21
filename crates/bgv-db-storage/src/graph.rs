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
//! this index may serve it — see `docs/bgvql.md`. Silence gets the scan.
//!
//! # Deletion, and where the decay lives
//!
//! Removing a record removes its node and the edges out of it. The edges **into**
//! it are left, because finding them means reading every node that might point
//! here. Correctness is unaffected: an index read is a candidate set and each
//! candidate is resolved at the reader's own snapshot, so a removed record never
//! reaches an answer. Recall is affected, and decays with churn. The remedy is a
//! rebuild, which is not built.

use std::collections::{BTreeMap, BTreeSet};

use bgv_db_encoding::{IndexAddress, StoreKey, StoreValue, VectorNode, VectorNodeKey};
use bgv_db_kv::{KeyRange, ScanDirection, ScanRequest, WriteBatch};
use bgv_db_types::{Number, RecordId, Value};

use crate::catalog::VectorDistance;
use crate::error::Result;
use crate::store::Store;

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

/// The graph of one index, read from the committed state.
///
/// Held in memory for the length of one operation. That is a real limit and it
/// is stated rather than discovered: an index over more vectors than fit is a
/// paging walk, which is a different piece of work.
#[derive(Debug)]
pub(crate) struct Graph {
    nodes: BTreeMap<RecordId, VectorNode>,
    distance: VectorDistance,
}

impl Graph {
    /// An empty graph for this distance.
    pub(crate) fn empty(distance: VectorDistance) -> Self {
        Self {
            nodes: BTreeMap::new(),
            distance,
        }
    }

    /// Read every node of an index.
    pub(crate) fn read(
        store: &Store,
        address: &IndexAddress,
        distance: VectorDistance,
    ) -> Result<Self> {
        let prefix = VectorNodeKey::level_prefix(address, GROUND);
        let request = ScanRequest {
            keyspace: VectorNodeKey::keyspace(),
            range: KeyRange::prefix(&prefix),
            direction: ScanDirection::Forward,
            limit: None,
        };
        let mut nodes = BTreeMap::new();
        for (key, value) in store.backend().scan(&request)? {
            let decoded = VectorNodeKey::decode(key.as_slice())?;
            nodes.insert(decoded.id, VectorNode::decode(value.as_slice())?);
        }
        Ok(Self { nodes, distance })
    }

    /// Whether the graph holds nothing.
    pub(crate) fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// The records nearest this vector, nearest first, at most `wanted` of them.
    ///
    /// A greedy walk from the entry point, keeping the best [`EXPLORATION`]
    /// candidates seen. Approximate by construction — see the module
    /// documentation for why that is a language-level decision and not a detail.
    pub(crate) fn nearest(&self, query: &[f64], wanted: usize) -> Vec<RecordId> {
        let Some(entry) = self.entry() else {
            return Vec::new();
        };
        let mut seen: BTreeSet<RecordId> = BTreeSet::new();
        // Candidates to expand, and the best found so far. Both are kept sorted
        // by distance with the record id breaking ties, so the walk is a
        // function of the graph and the query and of nothing else.
        let mut frontier: Vec<(f64, RecordId)> = Vec::new();
        let mut best: Vec<(f64, RecordId)> = Vec::new();

        let start = self.at(entry.clone(), query);
        seen.insert(entry.clone());
        frontier.push((start, entry.clone()));
        best.push((start, entry));

        while let Some((_, current)) = take_nearest(&mut frontier) {
            let Some(node) = self.nodes.get(&current) else {
                continue;
            };
            // Stop when nothing in hand can improve on what is already held:
            // the classic greedy cut-off, and what keeps the walk sub-linear.
            if let Some((furthest, _)) = best.last() {
                if best.len() >= EXPLORATION && self.at(current.clone(), query) > *furthest {
                    break;
                }
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
                if !self.nodes.contains_key(neighbour) {
                    continue;
                }
                let distance = self.at(neighbour.clone(), query);
                frontier.push((distance, neighbour.clone()));
                insert_sorted(&mut best, distance, neighbour.clone(), EXPLORATION);
            }
        }

        best.into_iter().take(wanted).map(|(_, id)| id).collect()
    }

    /// How far this record is from the query, or infinitely far if it is gone.
    fn at(&self, id: RecordId, query: &[f64]) -> f64 {
        self.nodes.get(&id).map_or(f64::INFINITY, |node| {
            separation(self.distance, &node.vector, query)
        })
    }

    /// Where a walk starts.
    ///
    /// The smallest record id, so the entry point is a property of the data
    /// rather than of insertion order — which means it needs no key of its own
    /// and cannot drift out of step with the nodes.
    fn entry(&self) -> Option<RecordId> {
        self.nodes.keys().next().cloned()
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
    ) -> BTreeMap<RecordId, VectorNode> {
        let mut touched = BTreeMap::new();
        let chosen = if self.is_empty() {
            Vec::new()
        } else {
            self.nearest(&vector, NEIGHBOURS)
        };
        let node = VectorNode::new(vector.clone(), chosen.clone());
        self.nodes.insert(id.clone(), node.clone());
        touched.insert(id.clone(), node);

        for neighbour in chosen {
            let Some(held) = self.nodes.get(&neighbour) else {
                continue;
            };
            if held.neighbours.contains(id) {
                continue;
            }
            let mut linked = held.neighbours.clone();
            linked.push(id.clone());
            let pruned = self.prune(&held.vector.clone(), linked);
            let updated = VectorNode::new(held.vector.clone(), pruned);
            self.nodes.insert(neighbour.clone(), updated.clone());
            touched.insert(neighbour, updated);
        }
        touched
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
    fn prune(&self, from: &[f64], candidates: Vec<RecordId>) -> Vec<RecordId> {
        let mut ranked: Vec<(f64, RecordId)> = candidates
            .into_iter()
            .map(|id| (self.at(id.clone(), from), id))
            .collect();
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
            let Some(held) = self.nodes.get(&id) else {
                continue;
            };
            let covered = kept
                .iter()
                .any(|(_, other)| separation(self.distance, &held.vector, other) < to_base);
            if covered {
                continue;
            }
            kept.push((id, held.vector.clone()));
        }
        kept.into_iter().map(|(id, _)| id).collect()
    }

    /// Take a record out of the graph.
    pub(crate) fn remove(&mut self, id: &RecordId) {
        self.nodes.remove(id);
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

/// Remove one node from the batch.
pub(crate) fn erase(batch: WriteBatch, address: &IndexAddress, id: &RecordId) -> WriteBatch {
    batch.delete(
        VectorNodeKey::keyspace(),
        VectorNodeKey::new(*address, GROUND, id.clone()).encode(),
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic)]

    use bgv_db_types::{Number, RecordId, Value};

    use super::{Graph, VectorDistance, separation, vector_of};

    fn vector(components: &[f64]) -> Value {
        Value::Array(
            components
                .iter()
                .map(|held| Value::Number(Number::float(*held)))
                .collect(),
        )
    }

    fn built(points: &[(i64, [f64; 2])]) -> Graph {
        let mut graph = Graph::empty(VectorDistance::Euclidean);
        for (id, point) in points {
            graph.insert(&RecordId::Int(*id), point.to_vec());
        }
        graph
    }

    #[test]
    fn a_vector_is_read_the_same_way_the_distances_read_one() {
        // A record the index accepts and the distance refuses would be in the
        // graph and incomparable; the reverse would be comparable and absent.
        assert_eq!(vector_of(&vector(&[1.0, 2.0])), Some(vec![1.0, 2.0]));
        assert_eq!(vector_of(&vector(&[])), None);
        assert_eq!(vector_of(&Value::from("not a vector")), None);
        assert_eq!(
            vector_of(&Value::Array(vec![Value::from("x")])),
            None,
            "an array of something that is not a number is not a vector"
        );
        assert_eq!(vector_of(&Value::None), None);
    }

    #[test]
    fn a_squared_distance_orders_the_way_the_real_one_does() {
        // Which is the whole justification for not taking the root.
        let origin = [0.0, 0.0];
        let near = separation(VectorDistance::Euclidean, &origin, &[1.0, 0.0]);
        let far = separation(VectorDistance::Euclidean, &origin, &[3.0, 0.0]);
        assert!(near < far);
        assert!(separation(VectorDistance::Euclidean, &origin, &[1.0]).is_infinite());
        assert!(separation(VectorDistance::Euclidean, &[], &[]).is_infinite());
    }

    #[test]
    fn an_empty_graph_answers_with_nothing_rather_than_failing() {
        let graph = Graph::empty(VectorDistance::Euclidean);
        assert!(graph.is_empty());
        assert!(graph.nearest(&[1.0, 1.0], 10).is_empty());
    }

    #[test]
    fn the_walk_finds_the_nearest_on_a_line() {
        // Small enough that the exact answer is obvious by inspection, which is
        // what makes it a test of the walk rather than of a fixture.
        let graph = built(&[
            (1, [0.0, 0.0]),
            (2, [1.0, 0.0]),
            (3, [2.0, 0.0]),
            (4, [3.0, 0.0]),
            (5, [4.0, 0.0]),
        ]);
        let found = graph.nearest(&[0.1, 0.0], 2);
        assert_eq!(found, vec![RecordId::Int(1), RecordId::Int(2)]);
    }

    #[test]
    fn every_record_is_reachable_because_the_reverse_edge_is_written() {
        // Without it a new record links outward and nothing links back, so the
        // graph becomes a collection of one-way streets and a walk from the
        // entry point can never arrive.
        let graph = built(&[
            (1, [0.0, 0.0]),
            (2, [10.0, 0.0]),
            (3, [20.0, 0.0]),
            (4, [30.0, 0.0]),
        ]);
        for (id, point) in [(2_i64, [10.0, 0.0]), (3, [20.0, 0.0]), (4, [30.0, 0.0])] {
            let found = graph.nearest(&point, 1);
            assert_eq!(found, vec![RecordId::Int(id)], "could not reach {id}");
        }
    }

    #[test]
    fn the_same_records_in_the_same_order_build_the_same_graph() {
        // The property a replica depends on, and the reason there are no random
        // levels: two replicas replay one log and must agree about which records
        // are nearest.
        let points: Vec<(i64, [f64; 2])> = (0..40)
            .map(|n| {
                let held = f64::from(n);
                (i64::from(n), [held * 0.7, held * -0.3])
            })
            .collect();
        let first = built(&points);
        let second = built(&points);
        assert_eq!(first.nodes, second.nodes);
    }

    #[test]
    fn a_removed_record_is_no_longer_answered_with() {
        let mut graph = built(&[(1, [0.0, 0.0]), (2, [1.0, 0.0]), (3, [2.0, 0.0])]);
        graph.remove(&RecordId::Int(1));
        let found = graph.nearest(&[0.0, 0.0], 3);
        assert!(!found.contains(&RecordId::Int(1)), "{found:?}");
    }

    #[test]
    fn a_node_keeps_at_most_the_neighbours_it_is_allowed() {
        let points: Vec<(i64, [f64; 2])> = (0..60)
            .map(|n| (i64::from(n), [f64::from(n) * 0.1, 0.0]))
            .collect();
        let graph = built(&points);
        for (id, node) in &graph.nodes {
            assert!(
                node.neighbours.len() <= super::NEIGHBOURS,
                "{id} kept {}",
                node.neighbours.len()
            );
            assert!(!node.neighbours.contains(id), "{id} points at itself");
        }
    }

    /// A point near one of forty centres, the way a real embedding sits.
    ///
    /// The jitter is **wide and well mixed** on purpose. An earlier version took
    /// it modulo sixty, which made thousands of records share a vector exactly —
    /// and recall over duplicates measures which tie a sort broke, not whether a
    /// search found anything. It read as a broken index for an hour.
    fn clustered(n: i64, dimensions: usize) -> Vec<f64> {
        const CENTRES: i64 = 40;
        let centre = n % CENTRES;
        (0..dimensions)
            .map(|d| {
                let axis = i64::try_from(d).unwrap_or(0);
                let base = centre
                    .wrapping_mul(7_919)
                    .wrapping_add(axis.wrapping_mul(104_729))
                    .rem_euclid(1_000);
                let mut held = n
                    .wrapping_add(1)
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(axis.wrapping_mul(1_442_695_040_888_963_407));
                held ^= held >> 33;
                held = held.wrapping_mul(-49_064_778_989_728_563_i64);
                held ^= held >> 29;
                let jitter = held.rem_euclid(200).saturating_sub(100);
                let thousandths = base.saturating_add(jitter).rem_euclid(1_000);
                f64::from(i32::try_from(thousandths).unwrap_or(0)) / 1000.0
            })
            .collect()
    }

    #[test]
    fn the_walk_finds_nearly_all_of_the_true_nearest() {
        // The one thing this index cannot promise by construction, so it is
        // **measured** here and by the benchmark harness rather than asserted in
        // prose. Two thousand clustered points in thirty-two dimensions, the
        // exact ten computed by brute force, and the overlap counted.
        //
        // The floor is deliberately below what this fixture achieves: the number
        // to defend is "the search works", not "the search scores exactly what
        // it scored the day it was written".
        const RECORDS: i64 = 2_000;
        const DIMENSIONS: usize = 32;

        let mut graph = Graph::empty(VectorDistance::Euclidean);
        for n in 0..RECORDS {
            graph.insert(&RecordId::Int(n), clustered(n, DIMENSIONS));
        }

        let mut hit = 0_usize;
        let mut asked = 0_usize;
        for q in 0..20 {
            let query = clustered(RECORDS.saturating_add(q), DIMENSIONS);
            let mut exact: Vec<(f64, i64)> = (0..RECORDS)
                .map(|n| {
                    (
                        separation(VectorDistance::Euclidean, &clustered(n, DIMENSIONS), &query),
                        n,
                    )
                })
                .collect();
            exact.sort_by(|left, right| left.0.total_cmp(&right.0).then(left.1.cmp(&right.1)));
            let truth: Vec<RecordId> = exact
                .iter()
                .take(10)
                .map(|(_, n)| RecordId::Int(*n))
                .collect();
            let found = graph.nearest(&query, 10);
            hit = hit.saturating_add(found.iter().filter(|id| truth.contains(id)).count());
            asked = asked.saturating_add(truth.len());
        }
        let recall = hit.saturating_mul(100).checked_div(asked).unwrap_or(0);
        assert!(recall >= 90, "recall was {recall}%");
    }
}
