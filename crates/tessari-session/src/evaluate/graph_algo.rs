//! `FROM GRAPH … BY` — whole-graph analytics over a declared graph (ADR-0144).
//!
//! # Why a source and not a function
//!
//! A graph pass reads the whole structure once; a function in the catalog is
//! pure per-value. The source shape answers one row per node — `{ node,
//! score }` — under the reader's snapshot and grants, composable with the
//! ordinary clauses over the answer rows.
//!
//! # Determinism
//!
//! Nodes are ordered by (table, id); adjacency lists are sorted; pagerank and
//! label propagation run a fixed number of rounds. The same graph answers the
//! same rows on every run, whatever order the store read it in.
//!
//! # Where the CPU time goes
//!
//! The passes are synchronous computation over data the scan already read —
//! they run inside the statement's evaluation, bounded by the statement's
//! budget like any other read, and are not spawned anywhere.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use tessari_encoding::Direction as AdjacencyDirection;
use tessari_ql::{Name, RecordTarget, TableRef};
use tessari_storage::{Catalog, EdgeKindDefinition, RecordAddress, Transaction};
use tessari_types::{GraphId, Number, RecordId, RecordRef, TableId, Value};

use crate::context::Context;
use crate::error::{Error, Result};
use crate::session::Session;

/// The graph one pass reads: ordered nodes and sorted out-adjacency, built
/// once from the catalog and adjacency.
struct Scan {
    context: Context,
    /// (table, id) pairs in graph order — the index of a node is its position.
    nodes: Vec<(TableId, RecordId)>,
    /// Out-steps by node index, sorted ascending, deduped.
    out: Vec<Vec<usize>>,
}

impl Scan {
    fn position(&self, table: TableId, id: &RecordId) -> Option<usize> {
        self.nodes
            .iter()
            .position(|(held_table, held_id)| *held_table == table && held_id == id)
    }
}

impl Session<'_> {
    /// Run the algorithm a `FROM GRAPH … BY` names and answer one row per
    /// node: `{ node: <record>, score | cost: <number> }`.
    pub(super) async fn graph_read(
        &self,
        transaction: &mut Transaction<'_>,
        graph: &Name,
        algorithm: &Name,
        start: Option<&RecordTarget>,
        using: Option<&Name>,
    ) -> Result<Vec<(RecordId, Value)>> {
        if !transaction.indexes_are_current().await? {
            return Err(super::graph::behind_the_tail(
                transaction,
                graph.text.clone(),
                graph.span,
            ));
        }
        let (context, graph_id) = self.locate_graph(transaction, graph).await?;
        let kinds = self
            .graph_kinds(transaction, &context, graph_id, graph, using)
            .await?;
        let needs_start = algorithm.text.eq_ignore_ascii_case("sssp");
        if needs_start && start.is_none() {
            return Err(Error::AlgorithmNeedsStart {
                algorithm: algorithm.text.clone(),
                span: algorithm.span,
            });
        }
        if !needs_start && start.is_some() {
            return Err(Error::AlgorithmTakesNoStart {
                algorithm: algorithm.text.clone(),
                span: algorithm.span,
            });
        }
        let scan = build_scan(self, transaction, &context, &kinds, graph_id).await?;
        let scores: Vec<(usize, Number)> = match algorithm.text.to_ascii_lowercase().as_str() {
            "pagerank" => pagerank(&scan),
            "wcc" => components(&scan),
            "cdlp" => label_propagation(&scan),
            "betweenness" => betweenness(&scan),
            "harmonic" => harmonic(&scan),
            "sssp" => {
                let Some(start) = start else {
                    unreachable!("checked above");
                };
                let (_, start_address) = self.address(transaction, start).await?;
                let Some(start_at) = scan.position(start_address.table, &start_address.id) else {
                    return Ok(Vec::new());
                };
                single_source(&scan, start_at)
            }
            _ => {
                return Err(Error::UnknownAlgorithm {
                    name: algorithm.text.clone(),
                    span: algorithm.span,
                });
            }
        };
        self.algorithm_rows(transaction, &scan, scores, algorithm)
            .await
    }

    /// Resolve the graph name, refused by name when the catalog does not
    /// hold it.
    async fn locate_graph(
        &self,
        transaction: &mut Transaction<'_>,
        graph: &Name,
    ) -> Result<(Context, GraphId)> {
        let context = self.context(transaction, None, graph.span).await?;
        let Some(id) = Catalog::new(transaction)
            .graph_id(context.namespace, context.database, &graph.text)
            .await?
        else {
            return Err(Error::GraphNotDeclared {
                name: graph.text.clone(),
                span: graph.span,
            });
        };
        Ok((context, id))
    }

    /// The edge kinds the pass walks: the one `USING` names, refused when the
    /// graph does not declare it, or every kind of the graph.
    async fn graph_kinds(
        &self,
        transaction: &mut Transaction<'_>,
        context: &Context,
        graph_id: GraphId,
        graph: &Name,
        using: Option<&Name>,
    ) -> Result<Vec<EdgeKindDefinition>> {
        let kinds = Catalog::new(transaction)
            .edge_kinds_in(context.namespace, context.database)
            .await?;
        match using {
            Some(named) => kinds
                .into_iter()
                .find(|kind| kind.graph == graph_id && kind.name.eq_ignore_ascii_case(&named.text))
                .map(|kind| vec![kind])
                .ok_or_else(|| Error::EdgeKindNotInGraph {
                    kind: named.text.clone(),
                    graph: graph.text.clone(),
                    span: named.span,
                }),
            None => Ok(kinds
                .into_iter()
                .filter(|kind| kind.graph == graph_id)
                .collect()),
        }
    }

    /// The answer rows: one per scored node the reader may see, carrying the
    /// record reference and its score.
    async fn algorithm_rows(
        &self,
        transaction: &mut Transaction<'_>,
        scan: &Scan,
        scores: Vec<(usize, Number)>,
        algorithm: &Name,
    ) -> Result<Vec<(RecordId, Value)>> {
        let keyed = algorithm.text.eq_ignore_ascii_case("sssp");
        let mut rows = Vec::with_capacity(scores.len());
        for (at, score) in scores {
            let Some((table, id)) = scan.nodes.get(at) else {
                continue;
            };
            let visible = self.visible_in(transaction, *table).await?;
            let guards = self.rows_in(transaction, *table).await?;
            let address = RecordAddress::new(
                scan.context.namespace,
                scan.context.database,
                *table,
                id.clone(),
            );
            let Some(payload) = transaction.get(&address).await? else {
                continue;
            };
            let record = self.record_of(&payload, &visible)?;
            let Some(_record) = self
                .permitted(transaction, id, record, &visible, &guards)
                .await?
            else {
                continue;
            };
            let mut fields = BTreeMap::new();
            fields.insert(
                "node".to_owned(),
                Value::Record(RecordRef::new(*table, id.clone())),
            );
            fields.insert(
                if keyed { "cost" } else { "score" }.to_owned(),
                Value::Number(score),
            );
            rows.push((id.clone(), Value::Object(fields)));
        }
        Ok(rows)
    }
}

/// Read the graph once: every node of the graph's member tables, and the
/// sorted, deduped out-steps through the kinds the pass walks.
async fn build_scan(
    session: &Session<'_>,
    transaction: &mut Transaction<'_>,
    context: &Context,
    kinds: &[EdgeKindDefinition],
    graph_id: GraphId,
) -> Result<Scan> {
    let catalog = Catalog::new(transaction);
    let mut nodes: Vec<(TableId, RecordId)> = Vec::new();
    for table in catalog
        .tables_in(context.namespace, context.database)
        .await?
    {
        if table.graph != Some(graph_id) {
            continue;
        }
        // Resolving through the statement's own path enforces the table's
        // grant, as a walk's hop does.
        session
            .resolve_table(
                transaction,
                &TableRef {
                    database: None,
                    name: Name {
                        text: table.name.clone(),
                        span: graph_span(),
                    },
                    span: graph_span(),
                },
            )
            .await?;
        for (id, _) in transaction
            .scan_table(context.namespace, context.database, table.id)
            .await?
        {
            nodes.push((table.id, id));
        }
    }
    nodes.sort();
    nodes.dedup();
    let mut out: Vec<BTreeSet<usize>> = vec![BTreeSet::new(); nodes.len()];
    for kind in kinds {
        for (at, (table, id)) in nodes.iter().enumerate() {
            for neighbour in transaction
                .neighbours(kind, *table, id, AdjacencyDirection::Out)
                .await?
            {
                let Some(far) = nodes.iter().position(|(held_table, held_id)| {
                    *held_table == neighbour.table && held_id == &neighbour.id
                }) else {
                    continue;
                };
                out[at].insert(far);
            }
        }
    }
    let out = out
        .into_iter()
        .map(|set| set.into_iter().collect())
        .collect();
    Ok(Scan {
        context: *context,
        nodes,
        out,
    })
}

/// The fixed pagerank rounds: damping 0.85, twenty iterations, dangling nodes
/// distributing their rank across the graph. Deterministic by construction.
fn pagerank(scan: &Scan) -> Vec<(usize, Number)> {
    let count = scan.nodes.len().max(1);
    let share = 1.0 / f64::from(u32::try_from(count).unwrap_or(u32::MAX)).max(1.0);
    let mut rank = vec![share; scan.nodes.len()];
    for _ in 0..20 {
        let dangling: f64 = rank
            .iter()
            .zip(&scan.out)
            .filter(|(_, out)| out.is_empty())
            .map(|(held, _)| *held)
            .sum();
        let mut next = vec![share * (1.0 - 0.85); scan.nodes.len()];
        for (at, out) in scan.out.iter().enumerate() {
            if out.is_empty() {
                continue;
            }
            let spread =
                rank[at] * 0.85 / f64::from(u32::try_from(out.len()).unwrap_or(u32::MAX)).max(1.0);
            for far in out {
                next[*far] += spread;
            }
        }
        for held in &mut next {
            *held += 0.85 * dangling / f64::from(u32::try_from(count).unwrap_or(u32::MAX)).max(1.0);
        }
        rank = next;
    }
    rank.into_iter()
        .enumerate()
        .map(|(at, held)| (at, Number::float(held)))
        .collect()
}

/// Weakly-connected components by union over both directions; the label is
/// the component's smallest member's position in the graph's node order.
fn components(scan: &Scan) -> Vec<(usize, Number)> {
    fn find(parent: &mut [usize], at: usize) -> usize {
        if parent[at] != at {
            let root = find(parent, parent[at]);
            parent[at] = root;
        }
        parent[at]
    }
    let mut parent: Vec<usize> = (0..scan.nodes.len()).collect();
    for (at, out) in scan.out.iter().enumerate() {
        for far in out {
            let (left, right) = (find(&mut parent, at), find(&mut parent, *far));
            parent[left.max(right)] = left.min(right);
        }
    }
    let mut label: BTreeMap<usize, usize> = BTreeMap::new();
    (0..scan.nodes.len())
        .map(|at| {
            let root = find(&mut parent, at);
            let held = *label.entry(root).or_insert(root);
            (at, Number::Integer(i64::try_from(held).unwrap_or(i64::MAX)))
        })
        .collect()
}

/// Synchronous label propagation: ten rounds of every node taking the
/// smallest label among itself and its neighbours, both directions. The
/// initial label is the node's own position, so the result is a function of
/// the graph alone.
fn label_propagation(scan: &Scan) -> Vec<(usize, Number)> {
    let mut incoming: Vec<Vec<usize>> = vec![Vec::new(); scan.nodes.len()];
    for (at, out) in scan.out.iter().enumerate() {
        for far in out {
            incoming[*far].push(at);
        }
    }
    let mut labels: Vec<usize> = (0..scan.nodes.len()).collect();
    for _ in 0..10 {
        let previous = labels.clone();
        for (at, label) in labels.iter_mut().enumerate() {
            let mut best = previous[at];
            for far in scan.out[at].iter().chain(&incoming[at]) {
                best = best.min(previous[*far]);
            }
            *label = best;
        }
    }
    labels
        .into_iter()
        .enumerate()
        .map(|(at, held)| (at, Number::Integer(i64::try_from(held).unwrap_or(i64::MAX))))
        .collect()
}

/// Brandes betweenness over the directed out-steps, unweighted. The count is
/// kept raw — the answer rows are a ranking, and a caller that wants a
/// normalization can fold one over the rows.
fn betweenness(scan: &Scan) -> Vec<(usize, Number)> {
    let count = scan.nodes.len();
    let mut centrality = vec![0.0_f64; count];
    for source in 0..count {
        let mut stack: Vec<usize> = Vec::new();
        let mut predecessors: Vec<Vec<usize>> = vec![Vec::new(); count];
        let mut sigma = vec![0.0_f64; count];
        let mut distance: Vec<Option<usize>> = vec![None; count];
        sigma[source] = 1.0;
        distance[source] = Some(0);
        let mut queue = VecDeque::from([source]);
        while let Some(at) = queue.pop_front() {
            stack.push(at);
            for far in &scan.out[at] {
                let step = distance[at].map_or(usize::MAX, |held| held.saturating_add(1));
                match distance[*far] {
                    None => {
                        distance[*far] = Some(step);
                        queue.push_back(*far);
                    }
                    Some(held) if held == step => {}
                    _ => continue,
                }
                sigma[*far] += sigma[at];
                predecessors[*far].push(at);
            }
        }
        let mut dependency = vec![0.0_f64; count];
        while let Some(at) = stack.pop() {
            for predecessor in &predecessors[at] {
                let ratio = if sigma[at] == 0.0 {
                    0.0
                } else {
                    sigma[*predecessor] / sigma[at] * (1.0 + dependency[at])
                };
                dependency[*predecessor] += ratio;
            }
            if at != source {
                centrality[at] += dependency[at];
            }
        }
    }
    centrality
        .into_iter()
        .enumerate()
        .map(|(at, held)| (at, Number::float(held)))
        .collect()
}

/// Harmonic centrality: the sum of `1/distance` over every node reachable
/// from the node itself.
fn harmonic(scan: &Scan) -> Vec<(usize, Number)> {
    (0..scan.nodes.len())
        .map(|source| {
            let mut seen: Vec<Option<usize>> = vec![None; scan.nodes.len()];
            seen[source] = Some(0);
            let mut queue = VecDeque::from([source]);
            let mut sum = 0.0_f64;
            while let Some(at) = queue.pop_front() {
                let depth = seen[at].unwrap_or(0);
                for far in &scan.out[at] {
                    if seen[*far].is_none() {
                        let next = depth.saturating_add(1);
                        seen[*far] = Some(next);
                        sum += 1.0
                            / f64::from(u32::try_from(next.max(1)).unwrap_or(u32::MAX)).max(1.0);
                        queue.push_back(*far);
                    }
                }
            }
            (source, Number::float(sum))
        })
        .collect()
}

/// Fewest-steps cost from the start, breadth-first; unreachable nodes are not
/// in the answer.
fn single_source(scan: &Scan, start: usize) -> Vec<(usize, Number)> {
    let mut distance: Vec<Option<usize>> = vec![None; scan.nodes.len()];
    distance[start] = Some(0);
    let mut queue = VecDeque::from([start]);
    while let Some(at) = queue.pop_front() {
        for far in &scan.out[at] {
            if distance[*far].is_none() {
                distance[*far] = distance[at].map(|held| held.saturating_add(1));
                queue.push_back(*far);
            }
        }
    }
    distance
        .into_iter()
        .enumerate()
        .filter_map(|(at, held)| {
            held.map(|steps| {
                (
                    at,
                    Number::Integer(i64::try_from(steps).unwrap_or(i64::MAX)),
                )
            })
        })
        .collect()
}

/// The span a catalog-derived table reference reports errors at: the graph
/// read's own statement is the only source there is, so a synthetic zero
/// span names it as precisely as anything else can.
fn graph_span() -> tessari_ql::Span {
    tessari_ql::Span { start: 0, end: 0 }
}
