//! Traversing edges and graphs.

use std::collections::{BTreeMap, BTreeSet};

use tessari_encoding::Direction as AdjacencyDirection;
use tessari_ql::{Direction, Hop, RecordTarget};
use tessari_storage::{Catalog, EDGE_IN, EDGE_OUT, RecordAddress, Transaction};
use tessari_types::{Path, RecordId, RecordRef, TableId, Value};

use crate::error::{Error, Result};
use crate::session::Session;

use super::Hopped;

impl Session<'_> {
    /// A walk along one or more edge tables.
    ///
    /// Every step is an index read. The edge table was given an index on each
    /// endpoint when it was declared, so finding the edges out of a record is
    /// `records_by_index` on `out` — the same call an equality filter makes, with
    /// a record reference standing where any other value would.
    ///
    /// # A chain is one step repeated, and the repetition is where the care is
    ///
    /// Each step reads the edges out of **every** anchor it was handed, so a
    /// walk's cost multiplies by the branching factor at each hop. That is a real
    /// cost and `docs/tessariql.md` §4a states it rather than leaving it to be found.
    ///
    /// **The landing is deduplicated by record id**, which matters from the
    /// second hop onward and cannot happen on the first: two of ada's follows may
    /// follow one person, and this store's answers are keyed by record, so
    /// answering that person twice is a wrong answer rather than a verbose one.
    ///
    /// **A cycle is data.** The number of steps is written in the statement, so a
    /// walk cannot run away; if the walk arrives back where it started, that is
    /// the true answer to what was asked and not something to filter out.
    ///
    /// A dangling far endpoint drops that path rather than raising. A record can
    /// be deleted while an edge still names it, and that is a state of the graph,
    /// not a failure of the query — the alternative is a read that breaks because
    /// of a write it has nothing to do with.
    pub(super) fn traverse(
        &self,
        transaction: &mut Transaction<'_>,
        from: &RecordTarget,
        direction: Direction,
        hops: &[Hop],
        depth: Option<u64>,
    ) -> Result<Vec<(RecordId, Value)>> {
        // Refused rather than served, and refused here rather than at the index
        // lookup below, which would report `NotAnEdgeTable` and send a reader
        // looking at their schema for a fault that is not there.
        //
        // A traversal is the one read of the six that has no scan to fall back
        // to: an edge table's direction indexes are how edges are followed, not
        // an optimisation over following them, so `index_on_path` returning
        // `None` is treated as catalog corruption everywhere else. Inventing a
        // scan for the historical case would be a second traversal
        // implementation with its own direction handling, built inside a wave
        // whose subject is guarding the paths that already exist.
        //
        // Serving it from the present-day index is the alternative that must not
        // happen: a traversal's answer is the least inspectable shape this
        // language produces — a set of records reached through edges nobody
        // sees — so an answer assembled from today's edges over yesterday's
        // records would be believed.
        if !transaction.indexes_are_current()? {
            return Err(Error::NoHistoricalTraversal {
                table: hops
                    .first()
                    .map_or_else(String::new, |hop| hop.edges.name.text.clone()),
                span: from.span,
            });
        }
        let (origin, start) = self.address(transaction, from)?;
        let anchors = vec![RecordRef::new(start.table, start.id.clone())];
        match depth {
            None => self.walk_written_out(transaction, &origin, anchors, direction, hops),
            // The parser has already established that there is exactly one hop
            // and that it names the table it lands on, so this indexes rather
            // than re-checking: a second copy of the rule is a second place for
            // it to disagree with itself.
            Some(limit) => {
                self.walk_repeatedly(transaction, &origin, anchors, direction, &hops[0], limit)
            }
        }
    }

    /// A walk whose steps are written out, one hop each.
    ///
    /// Bounded because the statement says how many steps there are — which is
    /// why this form needs no visited set and may legitimately answer with the
    /// same record twice if two written steps reach it.
    pub(super) fn walk_written_out(
        &self,
        transaction: &mut Transaction<'_>,
        origin: &crate::context::Context,
        mut anchors: Vec<RecordRef>,
        direction: Direction,
        hops: &[Hop],
    ) -> Result<Vec<(RecordId, Value)>> {
        let mut answer = Vec::new();
        for hop in hops {
            let (found, next) = self.one_hop(transaction, origin, &anchors, direction, hop)?;
            answer = found;
            anchors = next;
            // The last step named no node, so the edges themselves are the
            // answer. The grammar allows this only at the end, so there is no
            // case here where the walk would have had to continue.
            if hop.target.is_none() {
                break;
            }
        }
        Ok(answer)
    }

    /// `DEPTH n` — one hop repeated, answering with everything within `n` steps.
    ///
    /// Breadth-first over a visited set, and **the visited set is what makes the
    /// bound mean anything.** `n` is a literal, so the number of rounds is
    /// bounded by the statement; without the set, a cycle would make the *work*
    /// grow with `n` regardless — the walk would keep re-expanding records it
    /// had already reached, and a graph with one loop in it would run for as
    /// long as the number said. With the set each record is expanded once, so
    /// the walk costs the reachable subgraph however large `n` is written, and
    /// terminates on a cycle rather than on the count running out.
    ///
    /// The start is marked seen before the first round. That is not a special
    /// case for the start; it is the same rule, and it happens to give the
    /// answer people mean — a neighbourhood that contained its own centre would
    /// make a count of it wrong, and `SELECT * FROM person:1` already says the
    /// centre.
    ///
    /// Breadth-first rather than depth-first for the same reason: a record first
    /// reached in `d` steps has every neighbour of its own reached by `d + 1`,
    /// so expanding it again from a longer path can add nothing.
    pub(super) fn walk_repeatedly(
        &self,
        transaction: &mut Transaction<'_>,
        origin: &crate::context::Context,
        start: Vec<RecordRef>,
        direction: Direction,
        hop: &Hop,
        limit: u64,
    ) -> Result<Vec<(RecordId, Value)>> {
        let mut seen: BTreeSet<(TableId, RecordId)> = start
            .iter()
            .map(|anchor| (anchor.table, anchor.id.clone()))
            .collect();
        let mut frontier = start;
        let mut answer = Vec::new();
        for _ in 0..limit {
            if frontier.is_empty() {
                break;
            }
            let (found, next) = self.one_hop(transaction, origin, &frontier, direction, hop)?;
            // One pass, and the answer is filtered against a SET rather than
            // against a scan of the round's own results. A round's frontier is a
            // node's whole neighbourhood, so a linear search per record reached
            // would make the round quadratic in the fan-out — in the one code
            // path whose entire purpose is that a hop does not cost the degree.
            let mut fresh = BTreeSet::new();
            let mut next_frontier = Vec::new();
            for anchor in next {
                if seen.insert((anchor.table, anchor.id.clone())) {
                    fresh.insert(anchor.id.clone());
                    next_frontier.push(anchor);
                }
            }
            answer.extend(found.into_iter().filter(|(id, _)| fresh.contains(id)));
            frontier = next_frontier;
        }
        Ok(answer)
    }

    /// One step of a walk, whichever path serves it.
    ///
    /// Factored out of the loop so that `DEPTH` can take the same step more than
    /// once. Both branches answer with the same pair — the records this step
    /// reached, and the anchors the next step would start from — so a repeat is
    /// the same call again and not a second traversal implementation.
    pub(super) fn one_hop(
        &self,
        transaction: &mut Transaction<'_>,
        origin: &crate::context::Context,
        anchors: &[RecordRef],
        direction: Direction,
        hop: &Hop,
    ) -> Result<Hopped> {
        // An edge kind is looked for first, because it is served by
        // adjacency rather than by an index: the neighbours of one node under
        // one kind in one direction are contiguous, so reaching them is one
        // range read instead of an index probe and a random read of every
        // edge record. The far records are still read individually — the
        // caller asked for records — but the edges themselves are never
        // touched, and that is the difference the layout buys.
        if let Some(kind) = Catalog::new(transaction).edge_kind_id(
            origin.namespace,
            origin.database,
            &hop.edges.name.text,
        )? {
            return self.hop_in_graph(transaction, kind, anchors, direction, hop);
        }
        let (_, edge_table) = self.resolve_table(transaction, &hop.edges)?;
        if !Catalog::new(transaction)
            .table(edge_table)?
            .is_some_and(|found| found.is_edge())
        {
            return Err(Error::NotAnEdgeTable {
                table: hop.edges.name.text.clone(),
                span: hop.edges.span,
            });
        }
        edges_settled(transaction, edge_table, &hop.edges)?;
        let Some(index) = self.index_on_path(
            transaction,
            edge_table,
            &Path::field(direction.from_field()),
        )?
        else {
            // An edge table always has both, so reaching here means the
            // catalog and the flag disagree — which is corruption, not a
            // slow path.
            return Err(Error::NotAnEdgeTable {
                table: hop.edges.name.text.clone(),
                span: hop.edges.span,
            });
        };

        let edge_visible = self.visible_in(transaction, edge_table)?;
        let mut found = Vec::new();
        for anchor in anchors {
            let value = Value::Record(anchor.clone());
            let offered = transaction.records_by_index(&index, &[value])?;
            found.extend(self.records_of(offered, &edge_visible)?);
        }

        let Some(target) = hop.target.as_ref() else {
            // This step named no node, so the edges themselves are the answer
            // and there is nothing for a next step to start from.
            return Ok((found, Vec::new()));
        };

        let (context, target_table) = self.resolve_table(transaction, target)?;
        // The far side is read from its own table, so its own grant applies —
        // reaching a record through an edge is not a way around one, and that
        // holds at every hop rather than only at the first.
        let far_visible = self.visible_in(transaction, target_table)?;
        let mut reached: BTreeMap<RecordId, Value> = BTreeMap::new();
        for (_, edge) in found {
            let Value::Object(fields) = &edge else {
                continue;
            };
            let Some(Value::Record(far)) = fields.get(direction.to_field()) else {
                continue;
            };
            if far.table != target_table {
                continue;
            }
            let address = RecordAddress::new(
                context.namespace,
                context.database,
                target_table,
                far.id.clone(),
            );
            if let Some(payload) = transaction.get(&address)? {
                reached.insert(far.id.clone(), self.record_of(&payload, &far_visible)?);
            }
        }
        let next = reached
            .keys()
            .map(|id| RecordRef::new(target_table, id.clone()))
            .collect();
        Ok((reached.into_iter().collect(), next))
    }

    /// One hop over adjacency, and the anchors the next hop starts from.
    ///
    /// The neighbours come from a single range read per anchor. When the step
    /// names no node the edges themselves are the answer, and they are assembled
    /// from the adjacency entry rather than fetched: the endpoints are in the key
    /// and the properties are in the value, so an edge of a declared kind is
    /// never read as a record on this path at all.
    pub(super) fn hop_in_graph(
        &self,
        transaction: &mut Transaction<'_>,
        kind: tessari_types::EdgeKindId,
        anchors: &[RecordRef],
        direction: Direction,
        hop: &Hop,
    ) -> Result<Hopped> {
        let declared =
            Catalog::new(transaction)
                .edge_kind(kind)?
                .ok_or_else(|| Error::NotAnEdgeTable {
                    table: hop.edges.name.text.clone(),
                    span: hop.edges.span,
                })?;
        edges_settled(transaction, declared.edges, &hop.edges)?;
        let along = match direction {
            Direction::Outgoing => AdjacencyDirection::Out,
            Direction::Incoming => AdjacencyDirection::In,
        };

        let mut edges = Vec::new();
        for anchor in anchors {
            for neighbour in transaction.neighbours(&declared, anchor.table, &anchor.id, along)? {
                edges.push((anchor.clone(), neighbour));
            }
        }

        let Some(target) = hop.target.as_ref() else {
            let answer = edges
                .into_iter()
                .map(|(anchor, neighbour)| {
                    let mut fields = match neighbour.properties {
                        Value::Object(given) => given,
                        _ => BTreeMap::new(),
                    };
                    let (out, into) = match direction {
                        Direction::Outgoing => (
                            RecordRef::new(anchor.table, anchor.id.clone()),
                            RecordRef::new(neighbour.table, neighbour.id.clone()),
                        ),
                        Direction::Incoming => (
                            RecordRef::new(neighbour.table, neighbour.id.clone()),
                            RecordRef::new(anchor.table, anchor.id.clone()),
                        ),
                    };
                    let id = RecordId::from(format!(
                        "{}:{}->{}:{}",
                        out.table, out.id, into.table, into.id
                    ));
                    fields.insert(EDGE_OUT.to_owned(), Value::Record(out));
                    fields.insert(EDGE_IN.to_owned(), Value::Record(into));
                    (id, Value::Object(fields))
                })
                .collect();
            return Ok((answer, Vec::new()));
        };

        let (context, target_table) = self.resolve_table(transaction, target)?;
        // The far side is read from its own table, so its own grant applies —
        // reaching a record through an edge is not a way around one.
        let far_visible = self.visible_in(transaction, target_table)?;
        let mut reached: BTreeMap<RecordId, Value> = BTreeMap::new();
        for (_, neighbour) in edges {
            if neighbour.table != target_table {
                continue;
            }
            let address = RecordAddress::new(
                context.namespace,
                context.database,
                target_table,
                neighbour.id.clone(),
            );
            // A deleted neighbour drops out of the walk rather than failing it,
            // as it does on the edge-table path: a record can go while an entry
            // still names it, and that is a state of the graph.
            if let Some(payload) = transaction.get(&address)? {
                reached.insert(
                    neighbour.id.clone(),
                    self.record_of(&payload, &far_visible)?,
                );
            }
        }
        let next = reached
            .keys()
            .map(|id| RecordRef::new(target_table, id.clone()))
            .collect();
        Ok((reached.into_iter().collect(), next))
    }
}

/// Refuse a hop over `table` while its edges are indexed at another moment
/// than the reader sees them.
///
/// A traversal checks its snapshot's position once, at its start; this is the
/// other half, per edge table: a transaction across leaders part-way in it
/// here, whose edges the table's indexes and its readers disagree about
/// (Q-919). A traversal has no scan to fall back to, so either is a refusal.
pub(super) fn edges_settled(
    transaction: &Transaction<'_>,
    table: TableId,
    named: &tessari_ql::TableRef,
) -> Result<()> {
    if transaction.indexes_are_current_for(table)? {
        return Ok(());
    }
    let table = named.name.text.clone();
    let span = named.span;
    Err(if transaction.indexes_are_current()? {
        Error::AcrossSettling { table, span }
    } else {
        Error::NoHistoricalTraversal { table, span }
    })
}
