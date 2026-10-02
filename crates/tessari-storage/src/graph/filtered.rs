//! A nearest-neighbour walk that answers only with records a condition admits.
//!
//! # Why the walk navigates through records it will not answer with
//!
//! Searching first and filtering the answer afterwards collapses when the
//! condition is selective: the ten nearest records may hold none that match,
//! and the read answers with nothing while matching records exist. Restricting
//! the walk to admitted nodes collapses the other way — the graph was built over
//! every record, so its admitted part is a collection of islands a greedy walk
//! cannot cross.
//!
//! So the walk does both halves separately. **Navigation** follows every edge,
//! exactly as the unfiltered walk does; the **answer** keeps only the records
//! the condition admits. The cost of a selective condition is then more nodes
//! visited, never a wrong neighbourhood.
//!
//! # The condition is asked lazily, and at most once per record
//!
//! Admitting a record means reading it at the reader's snapshot and testing the
//! whole condition — far dearer than a distance. A node is asked only when it
//! could enter the answer as it stands, and every node is seen once, so the
//! condition runs on a fraction of what the walk visits.
//!
//! # A ceiling, and what reaching it means
//!
//! A condition admitting one record in ten thousand would have the walk visit
//! most of the graph to fill a page. The walk stops after [`FILTERED_REACH`] ×
//! its budget expansions and says it was cut; the caller then answers exactly,
//! because a filtered read that came back short is not one the statement's
//! `APPROXIMATE` agreed to.

use std::collections::BTreeSet;

use tessari_types::RecordId;

use super::{EXPLORATION, Graph, insert_sorted, take_nearest};

/// How many expansions a filtered walk may make, per candidate it keeps.
///
/// At the engine's own budget of [`EXPLORATION`] that is two thousand and
/// forty-eight nodes: enough to fill a page through a condition admitting about
/// one record in a hundred, and a bound past which the exact read — which
/// visits only the records the condition selects — is the cheaper honest answer.
pub(crate) const FILTERED_REACH: usize = 32;

/// How many expansions a filtered walk may make before it is cut.
///
/// One rule for the walk and for the caller deciding whether a set of
/// candidates is already small enough to answer exactly.
#[must_use]
pub fn filtered_ceiling(wanted: usize, effort: Option<usize>) -> usize {
    effort
        .unwrap_or(EXPLORATION)
        .max(wanted)
        .saturating_mul(FILTERED_REACH)
}

/// What a filtered walk found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Matched {
    /// The admitted records, nearest first, at most the number asked for.
    pub ids: Vec<RecordId>,
    /// Whether the walk stopped at its ceiling rather than by running out of
    /// anything nearer.
    pub cut: bool,
}

impl Graph {
    /// The admitted records nearest this vector, nearest first.
    ///
    /// The same deterministic best-first walk as [`Graph::nearest`], with the
    /// answer restricted to records `admit` accepts — see the module
    /// documentation. `admit` is asked at most once per record and only for a
    /// record that would enter the answer.
    ///
    /// # Errors
    ///
    /// Returns the first error `admit` returns; the walk itself cannot fail.
    pub fn nearest_matching<E>(
        &self,
        query: &[f64],
        wanted: usize,
        effort: Option<usize>,
        mut admit: impl FnMut(&RecordId) -> Result<bool, E>,
    ) -> Result<Matched, E> {
        let ceiling = filtered_ceiling(wanted, effort);
        let effort = effort.unwrap_or(EXPLORATION).max(wanted);
        let Some(entry) = self.entry() else {
            return Ok(Matched {
                ids: Vec::new(),
                cut: false,
            });
        };
        let mut seen: BTreeSet<RecordId> = BTreeSet::new();
        // Navigation and answer kept apart: every visited node may be expanded,
        // only an admitted one is held. Both sorted by distance, ties on id.
        let mut frontier: Vec<(f64, RecordId)> = Vec::new();
        let mut best: Vec<(f64, RecordId)> = Vec::new();
        let start = self.at(entry.clone(), query);
        seen.insert(entry.clone());
        if admit(&entry)? {
            best.push((start, entry.clone()));
        }
        frontier.push((start, entry));

        let mut expanded = 0_usize;
        let mut cut = false;
        while let Some((distance, current)) = take_nearest(&mut frontier) {
            // The same cut-off as the unfiltered walk, judged against admitted
            // records only: nothing nearer is left to admit.
            if let Some((furthest, _)) = best.last()
                && best.len() >= effort
                && distance > *furthest
            {
                break;
            }
            if expanded >= ceiling {
                cut = true;
                break;
            }
            expanded = expanded.saturating_add(1);
            let Some(node) = self.nodes.get(&current) else {
                continue;
            };
            for neighbour in &node.neighbours {
                if !seen.insert(neighbour.clone()) {
                    continue;
                }
                // A dangling edge (a removed record) neither routes nor answers.
                if !self.nodes.contains_key(neighbour) {
                    continue;
                }
                let separation = self.at(neighbour.clone(), query);
                frontier.push((separation, neighbour.clone()));
                // Asked only when the record would enter the answer as it stands.
                let could_enter = best.len() < effort
                    || best
                        .last()
                        .is_some_and(|(furthest, _)| separation < *furthest);
                if could_enter && admit(neighbour)? {
                    insert_sorted(&mut best, separation, neighbour.clone(), effort);
                }
            }
        }
        Ok(Matched {
            ids: best.into_iter().take(wanted).map(|(_, id)| id).collect(),
            cut,
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic)]

    use std::cell::Cell;
    use std::convert::Infallible;

    use tessari_types::RecordId;

    use super::super::{Graph, VectorDistance, separation};
    use super::{FILTERED_REACH, Matched};

    /// A point on a 40-centre cloud, deterministic in `n`.
    fn point(n: i64, dimensions: usize) -> Vec<f64> {
        (0..dimensions)
            .map(|d| {
                let axis = i64::try_from(d).unwrap_or(0);
                let mut held = n
                    .wrapping_add(1)
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(axis.wrapping_mul(1_442_695_040_888_963_407));
                held ^= held >> 33;
                held = held.wrapping_mul(-49_064_778_989_728_563_i64);
                held ^= held >> 29;
                let centre = n.rem_euclid(40).wrapping_mul(25);
                let thousandths = centre
                    .saturating_add(held.rem_euclid(200))
                    .rem_euclid(1_000);
                f64::from(i32::try_from(thousandths).unwrap_or(0)) / 1000.0
            })
            .collect()
    }

    fn graph_of(records: i64, dimensions: usize) -> Graph {
        let mut graph = Graph::empty(VectorDistance::Euclidean);
        for n in 0..records {
            graph.insert(&RecordId::Int(n), point(n, dimensions));
        }
        graph
    }

    /// The exact answer: every admitted record ranked by distance, ties on id.
    fn exact(
        records: i64,
        dimensions: usize,
        query: &[f64],
        wanted: usize,
        admits: impl Fn(i64) -> bool,
    ) -> Vec<RecordId> {
        let mut ranked: Vec<(f64, i64)> = (0..records)
            .filter(|n| admits(*n))
            .map(|n| {
                (
                    separation(VectorDistance::Euclidean, &point(n, dimensions), query),
                    n,
                )
            })
            .collect();
        ranked.sort_by(|left, right| left.0.total_cmp(&right.0).then(left.1.cmp(&right.1)));
        ranked
            .into_iter()
            .take(wanted)
            .map(|(_, n)| RecordId::Int(n))
            .collect()
    }

    fn admitting(modulus: i64) -> impl FnMut(&RecordId) -> Result<bool, Infallible> {
        move |id| Ok(matches!(id, RecordId::Int(n) if n.rem_euclid(modulus) == 0))
    }

    #[test]
    fn a_filtered_walk_answers_only_with_admitted_records() {
        let graph = graph_of(400, 8);
        let query = point(1_000, 8);
        let Ok(found) = graph.nearest_matching(&query, 10, None, admitting(3));
        assert_eq!(found.ids.len(), 10, "a third admitted still fills ten");
        for id in &found.ids {
            assert!(
                matches!(id, RecordId::Int(n) if n.rem_euclid(3) == 0),
                "{id:?}"
            );
        }
    }

    #[test]
    fn a_walk_that_reaches_every_record_answers_exactly() {
        // Three hundred records and a budget above them: the walk visits the
        // whole graph, so its answer must be the exact filtered answer — the
        // brute-force equivalence a filtered walk is held to where it cannot cut.
        let graph = graph_of(300, 6);
        for q in 0..5 {
            let query = point(5_000_i64.saturating_add(q), 6);
            let Ok(found) = graph.nearest_matching(&query, 10, Some(300), admitting(4));
            assert_eq!(
                found.ids,
                exact(300, 6, &query, 10, |n| n.rem_euclid(4) == 0)
            );
        }
    }

    #[test]
    fn the_condition_is_asked_at_most_once_per_record_and_not_for_every_record() {
        let graph = graph_of(2_000, 16);
        let query = point(9_000, 16);
        let asked = Cell::new(0_usize);
        let seen = std::cell::RefCell::new(std::collections::BTreeSet::new());
        let Ok(found) = graph.nearest_matching(&query, 10, None, |id: &RecordId| {
            asked.set(asked.get().saturating_add(1));
            assert!(seen.borrow_mut().insert(id.clone()), "{id:?} asked twice");
            Ok::<bool, Infallible>(true)
        });
        assert_eq!(found.ids.len(), 10);
        assert!(asked.get() < 2_000, "asked {} times", asked.get());
    }

    #[test]
    fn a_condition_almost_nothing_meets_cuts_the_walk_and_says_so() {
        // One record in five hundred is admitted: four in two thousand, and the
        // ceiling at a budget of ten is 320 expansions — too few to find ten.
        let graph = graph_of(2_000, 8);
        let query = point(7_000, 8);
        let Ok(found) = graph.nearest_matching(&query, 10, Some(10), admitting(500));
        assert!(found.cut, "{found:?}");
        assert!(found.ids.len() < 10);
        let _ = FILTERED_REACH;
    }

    #[test]
    fn a_failing_condition_ends_the_walk_with_its_error() {
        let graph = graph_of(200, 4);
        let answer: Result<Matched, &str> =
            graph.nearest_matching(&point(3_000, 4), 5, None, |_| Err("refused"));
        assert_eq!(answer, Err("refused"));
    }

    #[test]
    fn a_filtered_walk_finds_nearly_all_of_the_exact_filtered_nearest() {
        // The measured property, at three selectivities: half, a tenth and a
        // fiftieth of two thousand clustered records.
        let graph = graph_of(2_000, 32);
        for modulus in [2, 10, 50] {
            let (mut hit, mut asked) = (0_usize, 0_usize);
            for q in 0..20 {
                let query = point(20_000_i64.saturating_add(q), 32);
                let truth = exact(2_000, 32, &query, 10, |n| n.rem_euclid(modulus) == 0);
                let Ok(found) = graph.nearest_matching(&query, 10, None, admitting(modulus));
                hit = hit.saturating_add(found.ids.iter().filter(|id| truth.contains(id)).count());
                asked = asked.saturating_add(truth.len());
            }
            let recall = hit.saturating_mul(100).checked_div(asked).unwrap_or(0);
            assert!(recall >= 90, "1 in {modulus}: recall {recall}%");
        }
    }

    #[test]
    fn the_same_graph_and_query_walk_the_same_way() {
        let graph = graph_of(500, 8);
        let query = point(4_000, 8);
        let Ok(first) = graph.nearest_matching(&query, 10, None, admitting(7));
        let Ok(second) = graph.nearest_matching(&query, 10, None, admitting(7));
        assert_eq!(first, second);
    }
}
