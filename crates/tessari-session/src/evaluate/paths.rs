//! `PATH TO … DEPTH n [WEIGHT f]` — the shortest path in a declared graph.
//!
//! # One path, chosen by a rule the brute force can name
//!
//! Among the paths within `n` steps the answer is the cheapest, then the one with
//! fewest steps, then the one whose sequence of record ids is smallest. The
//! third rule is what makes the answer a function of the graph rather than of
//! the order the store happened to read it in.
//!
//! # The bound is on the work
//!
//! Both searches run over the subgraph reachable from the start within `n`
//! steps, each node read once. A hop-only path is breadth-first layers; a
//! weighted one is `n` rounds of "the cheapest way from here to the end within
//! `k` steps" — Bellman–Ford rounds, because a cap on steps is exactly what a
//! plain Dijkstra cannot honour: it returns the cheapest path, which may need
//! one step too many. The rounds stop as soon as one improves nothing.

use std::collections::{BTreeMap, BTreeSet};

use tessari_encoding::Direction as AdjacencyDirection;
use tessari_ql::{Direction, Hop, PathTo, RecordTarget};
use tessari_storage::{Catalog, EdgeKindDefinition, RecordAddress, Transaction};
use tessari_types::{Number, RecordId, TableId, Value};

use crate::context::Context;
use crate::error::{Error, Result};
use crate::outcome::Note;
use crate::session::Session;

/// The graph a path search reads: each node's out-steps, the far record and
/// what the step costs.
type Steps = BTreeMap<RecordId, Vec<(RecordId, f64)>>;

/// A path's records, start to end, and the note stating its steps and cost.
type Answered = (Vec<(RecordId, Value)>, Option<Note>);

/// What a path search reads its graph through.
struct Graph<'a> {
    kind: EdgeKindDefinition,
    along: AdjacencyDirection,
    context: Context,
    table: TableId,
    weight: Option<&'a str>,
    span: tessari_ql::Span,
    steps: Steps,
    exists: BTreeMap<RecordId, bool>,
}

impl Graph<'_> {
    /// The steps out of `node`: neighbours in the target table that exist, with
    /// their cost — read once per node.
    fn out_of(
        &mut self,
        transaction: &Transaction<'_>,
        node: &RecordId,
    ) -> Result<&[(RecordId, f64)]> {
        if !self.steps.contains_key(node) {
            let mut out = Vec::new();
            for neighbour in transaction.neighbours(&self.kind, self.table, node, self.along)? {
                if neighbour.table != self.table || neighbour.id == *node {
                    continue;
                }
                let Some(cost) = self.cost(&neighbour.properties)? else {
                    continue;
                };
                if self.alive(transaction, &neighbour.id)? {
                    out.push((neighbour.id, cost));
                }
            }
            out.sort_by(|left, right| left.0.cmp(&right.0));
            self.steps.insert(node.clone(), out);
        }
        Ok(self.steps.get(node).map_or(&[], Vec::as_slice))
    }

    /// What one step costs: one, or the edge's weight — `None` for an edge with
    /// no weight, which is no step at all for a weighted path.
    fn cost(&self, properties: &Value) -> Result<Option<f64>> {
        let Some(field) = self.weight else {
            return Ok(Some(1.0));
        };
        let held = match properties {
            Value::Object(fields) => fields.get(field),
            _ => None,
        };
        let refuse = |found: String| Error::PathWeight {
            field: field.to_owned(),
            found,
            span: self.span,
        };
        match held {
            None | Some(Value::None) => Ok(None),
            Some(Value::Number(number)) => match number.as_float() {
                Some(cost) if cost >= 0.0 && cost.is_finite() => Ok(Some(cost)),
                _ => Err(refuse(number.to_string())),
            },
            Some(other) => Err(refuse(format!("a {}", other.type_name()))),
        }
    }

    /// Whether a record is there: a deleted node drops out of every path, as it
    /// drops out of a `DEPTH` walk.
    fn alive(&mut self, transaction: &Transaction<'_>, node: &RecordId) -> Result<bool> {
        if let Some(known) = self.exists.get(node) {
            return Ok(*known);
        }
        let address = RecordAddress::new(
            self.context.namespace,
            self.context.database,
            self.table,
            node.clone(),
        );
        let alive = transaction.get(&address)?.is_some();
        self.exists.insert(node.clone(), alive);
        Ok(alive)
    }
}

impl Session<'_> {
    /// The shortest path from `from` to the record `path` names, within `depth`
    /// steps of `hop`, and the note stating its length and cost.
    pub(super) fn shortest_path(
        &self,
        transaction: &mut Transaction<'_>,
        (from, direction, hop, depth): (&RecordTarget, Direction, &Hop, u64),
        path: &PathTo,
    ) -> Result<Answered> {
        if !transaction.indexes_are_current()? {
            return Err(Error::NoHistoricalTraversal {
                table: hop.edges.name.text.clone(),
                span: from.span,
            });
        }
        let (origin, start) = self.address(transaction, from)?;
        let (_, end) = self.address(transaction, &path.to)?;
        let Some(kind) = Catalog::new(transaction).edge_kind_id(
            origin.namespace,
            origin.database,
            &hop.edges.name.text,
        )?
        else {
            // Resolved for its refusal: an unknown name is `Unknown`, a table
            // that exists is an edge table this search cannot walk backwards.
            self.resolve_table(transaction, &hop.edges)?;
            return Err(Error::PathOverEdgeTable {
                table: hop.edges.name.text.clone(),
                span: hop.edges.span,
            });
        };
        let kind =
            Catalog::new(transaction)
                .edge_kind(kind)?
                .ok_or_else(|| Error::NotAnEdgeTable {
                    table: hop.edges.name.text.clone(),
                    span: hop.edges.span,
                })?;
        super::graph::edges_settled(transaction, kind.edges, &hop.edges)?;
        let Some(target) = hop.target.as_ref() else {
            return Ok((Vec::new(), None));
        };
        let (context, table) = self.resolve_table(transaction, target)?;
        let mut graph = Graph {
            kind,
            along: match direction {
                Direction::Outgoing => AdjacencyDirection::Out,
                Direction::Incoming => AdjacencyDirection::In,
            },
            context,
            table,
            weight: path.weight.as_ref().map(|name| name.text.as_str()),
            span: path.span,
            steps: Steps::new(),
            exists: BTreeMap::new(),
        };
        if start.table != table
            || end.table != table
            || !graph.alive(transaction, &start.id)?
            || !graph.alive(transaction, &end.id)?
        {
            return Ok((Vec::new(), None));
        }
        let found = if graph.weight.is_some() {
            cheapest(transaction, &mut graph, &start.id, &end.id, depth)?
        } else {
            fewest(transaction, &mut graph, &start.id, &end.id, depth)?
        };
        let Some((ids, cost)) = found else {
            return Ok((Vec::new(), None));
        };
        let visible = self.visible_in(transaction, table)?;
        let mut records = Vec::with_capacity(ids.len());
        for id in &ids {
            let address =
                RecordAddress::new(context.namespace, context.database, table, id.clone());
            if let Some(payload) = transaction.get(&address)? {
                records.push((id.clone(), self.record_of(&payload, &visible)?));
            }
        }
        let steps = u64::try_from(ids.len().saturating_sub(1)).unwrap_or(u64::MAX);
        let cost = if graph.weight.is_some() {
            Number::Float(cost)
        } else {
            Number::Integer(i64::try_from(steps).unwrap_or(i64::MAX))
        };
        Ok((records, Some(Note::Path { steps, cost })))
    }
}

/// The fewest-steps path: breadth-first layers to the end, the nodes on a
/// shortest path marked backwards through them, and the smallest-id walk
/// forward through the marked ones.
fn fewest(
    transaction: &Transaction<'_>,
    graph: &mut Graph<'_>,
    start: &RecordId,
    end: &RecordId,
    depth: u64,
) -> Result<Option<(Vec<RecordId>, f64)>> {
    if start == end {
        return Ok(Some((vec![start.clone()], 0.0)));
    }
    let mut seen: BTreeSet<RecordId> = BTreeSet::from([start.clone()]);
    let mut layers: Vec<BTreeSet<RecordId>> = vec![BTreeSet::from([start.clone()])];
    let mut reached = false;
    for _ in 0..depth {
        let mut next = BTreeSet::new();
        let last = layers.last().cloned().unwrap_or_default();
        for node in &last {
            for (far, _) in graph.out_of(transaction, node)?.to_vec() {
                if seen.insert(far.clone()) {
                    next.insert(far);
                }
            }
        }
        if next.is_empty() {
            break;
        }
        reached = next.contains(end);
        layers.push(next);
        if reached {
            break;
        }
    }
    if !reached {
        return Ok(None);
    }
    // Backwards: which nodes of each layer lie on some shortest path.
    let mut useful: Vec<BTreeSet<RecordId>> = vec![BTreeSet::new(); layers.len()];
    if let Some(last) = useful.last_mut() {
        last.insert(end.clone());
    }
    for at in (0..layers.len().saturating_sub(1)).rev() {
        let mut marked = BTreeSet::new();
        for node in &layers[at] {
            let onward = graph.out_of(transaction, node)?;
            let next = useful.get(at.saturating_add(1));
            if onward
                .iter()
                .any(|(far, _)| next.is_some_and(|layer| layer.contains(far)))
            {
                marked.insert(node.clone());
            }
        }
        useful[at] = marked;
    }
    let mut walk = vec![start.clone()];
    let mut at = start.clone();
    for layer in useful.iter().skip(1) {
        let next = graph
            .out_of(transaction, &at)?
            .iter()
            .map(|(far, _)| far)
            .find(|far| layer.contains(*far))
            .cloned();
        let Some(next) = next else {
            return Ok(None);
        };
        walk.push(next.clone());
        at = next;
    }
    let cost = f64::from(u32::try_from(walk.len().saturating_sub(1)).unwrap_or(u32::MAX));
    Ok(Some((walk, cost)))
}

/// The cheapest path within `depth` steps: rounds of the cheapest cost to the
/// end within `k` steps from every node reachable from the start, then the
/// fewest `k` reaching the overall cheapest, then the smallest-id walk forward
/// that keeps the cost exact.
fn cheapest(
    transaction: &Transaction<'_>,
    graph: &mut Graph<'_>,
    start: &RecordId,
    end: &RecordId,
    depth: u64,
) -> Result<Option<(Vec<RecordId>, f64)>> {
    // The nodes reachable from the start within `depth`, each read once.
    let mut reachable: BTreeSet<RecordId> = BTreeSet::from([start.clone()]);
    let mut frontier = vec![start.clone()];
    for _ in 0..depth {
        let mut next = Vec::new();
        for node in &frontier {
            for (far, _) in graph.out_of(transaction, node)?.to_vec() {
                if reachable.insert(far.clone()) {
                    next.push(far);
                }
            }
        }
        if next.is_empty() {
            break;
        }
        frontier = next;
    }
    if !reachable.contains(end) {
        return Ok(None);
    }
    // rounds[k][v]: the cheapest cost from v to the end within k steps.
    let mut rounds: Vec<BTreeMap<RecordId, f64>> = vec![BTreeMap::from([(end.clone(), 0.0)])];
    for _ in 0..depth {
        let Some(previous) = rounds.last() else {
            break;
        };
        let mut current = previous.clone();
        for node in &reachable {
            for (far, cost) in graph.out_of(transaction, node)? {
                if let Some(onward) = previous.get(far) {
                    let through = cost + onward;
                    let held = current.entry(node.clone()).or_insert(f64::INFINITY);
                    if through < *held {
                        *held = through;
                    }
                }
            }
        }
        let settled = current == *previous;
        rounds.push(current);
        if settled {
            break;
        }
    }
    let Some(best) = rounds.last().and_then(|last| last.get(start)).copied() else {
        return Ok(None);
    };
    // The fewest steps reaching that cost.
    let Some(steps) = rounds
        .iter()
        .position(|round| round.get(start).is_some_and(|cost| *cost == best))
    else {
        return Ok(None);
    };
    let mut walk = vec![start.clone()];
    let mut at = start.clone();
    for left in (0..steps).rev() {
        let owed = rounds
            .get(left.saturating_add(1))
            .and_then(|round| round.get(&at))
            .copied()
            .unwrap_or(f64::INFINITY);
        let next = graph
            .out_of(transaction, &at)?
            .iter()
            .find(|(far, cost)| {
                rounds[left]
                    .get(far)
                    .is_some_and(|onward| cost + onward == owed)
            })
            .map(|(far, _)| far.clone());
        let Some(next) = next else {
            return Ok(None);
        };
        walk.push(next.clone());
        at = next;
    }
    Ok(Some((walk, best)))
}
