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
    pub(super) async fn traverse(
        &self,
        transaction: &mut Transaction<'_>,
        from: &RecordTarget,
        direction: Direction,
        hops: &[Hop],
        depth: Option<u64>,
        want_path: bool,
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
        if !transaction.indexes_are_current().await? {
            return Err(behind_the_tail(
                transaction,
                hops.first()
                    .map_or_else(String::new, |hop| hop.edges.name.text.clone()),
                from.span,
            ));
        }
        let (origin, start) = self.address(transaction, from).await?;
        let anchors = vec![RecordRef::new(start.table, start.id.clone())];
        // A segment changes the walk's contract from written steps to a
        // statement-wide seen set (ADR-0143), so it runs its own walker rather
        // than bending the written-out one.
        if hops.iter().any(|hop| hop.repeat.is_some()) {
            return self
                .walk_segmented(transaction, &origin, anchors, direction, hops, want_path)
                .await;
        }
        match depth {
            None => {
                self.walk_written_out(transaction, &origin, anchors, direction, hops)
                    .await
            }
            // The parser has already established that there is exactly one hop
            // and that it names the table it lands on, so this indexes rather
            // than re-checking: a second copy of the rule is a second place for
            // it to disagree with itself.
            Some(limit) => {
                self.walk_repeatedly(transaction, &origin, anchors, direction, &hops[0], limit)
                    .await
            }
        }
    }

    /// A walk whose steps are written out, one hop each.
    ///
    /// Bounded because the statement says how many steps there are — which is
    /// why this form needs no visited set and may legitimately answer with the
    /// same record twice if two written steps reach it.
    pub(super) async fn walk_written_out(
        &self,
        transaction: &mut Transaction<'_>,
        origin: &crate::context::Context,
        mut anchors: Vec<RecordRef>,
        direction: Direction,
        hops: &[Hop],
    ) -> Result<Vec<(RecordId, Value)>> {
        let mut answer = Vec::new();
        for hop in hops {
            let (found, next, _provenance) = self
                .one_hop(transaction, origin, &anchors, direction, hop)
                .await?;
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
    pub(super) async fn walk_repeatedly(
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
            let (found, next, _provenance) = self
                .one_hop(transaction, origin, &frontier, direction, hop)
                .await?;
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
    pub(super) async fn one_hop(
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
        if let Some(kind) = Catalog::new(transaction)
            .edge_kind_id(origin.namespace, origin.database, &hop.edges.name.text)
            .await?
        {
            return self
                .hop_in_graph(transaction, kind, anchors, direction, hop)
                .await;
        }
        let (_, edge_table) = self.resolve_table(transaction, &hop.edges).await?;
        if !Catalog::new(transaction)
            .table(edge_table)
            .await?
            .is_some_and(|found| found.is_edge())
        {
            return Err(Error::NotAnEdgeTable {
                table: hop.edges.name.text.clone(),
                span: hop.edges.span,
            });
        }
        // The segment is served by adjacency; an edge table is the
        // record-behind-an-index shape, and the refusal names that rather than
        // walking it one record at a time (ADR-0143).
        if hop.repeat.is_some() {
            return Err(Error::RepeatOnEdgeTable {
                table: hop.edges.name.text.clone(),
                span: hop
                    .repeat
                    .as_ref()
                    .map_or(hop.edges.span, |segment| segment.span),
            });
        }
        edges_settled(transaction, edge_table, &hop.edges).await?;
        let Some(index) = self
            .index_on_path(
                transaction,
                edge_table,
                &Path::field(direction.from_field()),
            )
            .await?
        else {
            // The lookup asks again whether the indexes are current, so a
            // commit landing since `edges_settled` answered also comes here,
            // and is refused for what it is (DB-V15).
            edges_settled(transaction, edge_table, &hop.edges).await?;
            // An edge table always has both, so reaching here means the
            // catalog and the flag disagree — which is corruption, not a
            // slow path.
            return Err(Error::NotAnEdgeTable {
                table: hop.edges.name.text.clone(),
                span: hop.edges.span,
            });
        };

        let edge_visible = self.visible_in(transaction, edge_table).await?;
        let mut found = Vec::new();
        for anchor in anchors {
            let value = Value::Record(anchor.clone());
            let offered = transaction.records_by_index(&index, &[value]).await?;
            found.extend(self.records_of(offered, &edge_visible)?);
        }

        let Some(target) = hop.target.as_ref() else {
            // This step named no node, so the edges themselves are the answer
            // and there is nothing for a next step to start from.
            return Ok((found, Vec::new(), Vec::new()));
        };

        let (context, target_table) = self.resolve_table(transaction, target).await?;
        // The far side is read from its own table, so its own grant applies —
        // reaching a record through an edge is not a way around one, and that
        // holds at every hop rather than only at the first.
        let far_visible = self.visible_in(transaction, target_table).await?;
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
            if let Some(payload) = transaction.get(&address).await? {
                reached.insert(far.id.clone(), self.record_of(&payload, &far_visible)?);
            }
        }
        let next = reached
            .keys()
            .map(|id| RecordRef::new(target_table, id.clone()))
            .collect();
        Ok((reached.into_iter().collect(), next, Vec::new()))
    }

    /// One hop over adjacency, and the anchors the next hop starts from.
    ///
    /// The neighbours come from a single range read per anchor. When the step
    /// names no node the edges themselves are the answer, and they are assembled
    /// from the adjacency entry rather than fetched: the endpoints are in the key
    /// and the properties are in the value, so an edge of a declared kind is
    /// never read as a record on this path at all.
    pub(super) async fn hop_in_graph(
        &self,
        transaction: &mut Transaction<'_>,
        kind: tessari_types::EdgeKindId,
        anchors: &[RecordRef],
        direction: Direction,
        hop: &Hop,
    ) -> Result<Hopped> {
        let declared = Catalog::new(transaction)
            .edge_kind(kind)
            .await?
            .ok_or_else(|| Error::NotAnEdgeTable {
                table: hop.edges.name.text.clone(),
                span: hop.edges.span,
            })?;
        edges_settled(transaction, declared.edges, &hop.edges).await?;
        let along = match direction {
            Direction::Outgoing => AdjacencyDirection::Out,
            Direction::Incoming => AdjacencyDirection::In,
        };

        let mut edges = Vec::new();
        let mut provenance: Vec<(RecordRef, RecordRef)> = Vec::new();
        for anchor in anchors {
            for neighbour in transaction
                .neighbours(&declared, anchor.table, &anchor.id, along)
                .await?
            {
                provenance.push((
                    RecordRef::new(neighbour.table, neighbour.id.clone()),
                    anchor.clone(),
                ));
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
            return Ok((answer, Vec::new(), provenance));
        };

        let (context, target_table) = self.resolve_table(transaction, target).await?;
        // The far side is read from its own table, so its own grant applies —
        // reaching a record through an edge is not a way around one.
        let far_visible = self.visible_in(transaction, target_table).await?;
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
            if let Some(payload) = transaction.get(&address).await? {
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
        Ok((reached.into_iter().collect(), next, provenance))
    }

    /// A chain that carries a segment: written steps and windows over one
    /// statement-wide seen set (ADR-0143).
    ///
    /// The seen set is what the segment buys: every record is expanded once
    /// across the whole walk, so a cycle terminates the work rather than
    /// feeding it, and a record reached by an early step is not re-answered by
    /// a later one. Each hop contributes only records first reached at its
    /// own step: a segment contributes the records first reached at a
    /// distance inside its window; a written step contributes what it freshly
    /// reached. The answer is the last hop's contribution, as it is for the
    /// written-out walk.
    ///
    /// `want_path` asks the walk to attach each answered record's first-reach
    /// chain under the `path` field: the route is contextual (ADR-0143), and
    /// the projection reads it like any other field on the answer.
    pub(super) async fn walk_segmented(
        &self,
        transaction: &mut Transaction<'_>,
        origin: &crate::context::Context,
        start: Vec<RecordRef>,
        direction: Direction,
        hops: &[Hop],
        want_path: bool,
    ) -> Result<Vec<(RecordId, Value)>> {
        let mut ledger = WalkLedger {
            seen: start
                .iter()
                .map(|anchor| (anchor.table, anchor.id.clone()))
                .collect(),
            parents: BTreeMap::new(),
        };
        let mut anchors = start;
        let mut answer: Vec<(RecordId, Value)> = Vec::new();
        for hop in hops {
            let (found, next) = if hop.repeat.is_some() {
                self.segment_rounds(transaction, origin, &mut ledger, anchors, direction, hop)
                    .await?
            } else {
                let (found, next, proven) = self
                    .one_hop(transaction, origin, &anchors, direction, hop)
                    .await?;
                for (child, parent) in proven {
                    ledger
                        .parents
                        .entry((child.table, child.id.clone()))
                        .or_insert((parent.table, parent.id.clone()));
                }
                let mut fresh = Vec::new();
                let mut fresh_ids = std::collections::BTreeSet::new();
                for anchor in next {
                    if ledger.seen.insert((anchor.table, anchor.id.clone())) {
                        fresh_ids.insert(anchor.id.clone());
                        fresh.push(anchor);
                    }
                }
                (
                    found
                        .into_iter()
                        .filter(|(id, _)| fresh_ids.contains(id))
                        .collect(),
                    fresh,
                )
            };
            answer = found;
            anchors = next;
        }
        if !want_path {
            return Ok(answer);
        }
        // Every record one hop lands on shares the hop's target table, so the
        // landing anchors name the table the answer's chains belong to.
        let Some(table) = anchors.first().map(|anchor| anchor.table) else {
            return Ok(answer);
        };
        let mut with_paths = Vec::with_capacity(answer.len());
        for (id, value) in answer {
            let Value::Object(mut fields) = value else {
                with_paths.push((id, value));
                continue;
            };
            let chain = reach_chain(&ledger.parents, (table, id.clone()));
            fields.insert("path".to_owned(), Value::Array(chain));
            with_paths.push((id, Value::Object(fields)));
        }
        Ok(with_paths)
    }

    /// The windowed expansion of one segment: the step taken `min..max` times,
    /// breadth-first, contributing the records first reached at a distance
    /// inside the window and returning that set as the next step's anchors.
    async fn segment_rounds(
        &self,
        transaction: &mut Transaction<'_>,
        origin: &crate::context::Context,
        ledger: &mut WalkLedger,
        anchors: Vec<RecordRef>,
        direction: Direction,
        hop: &Hop,
    ) -> Result<(Vec<(RecordId, Value)>, Vec<RecordRef>)> {
        // The parser has already established the segment's shape (ADR-0143), so
        // this indexes rather than re-checks — a second copy of the rule would
        // be a second place for it to disagree with itself.
        let Some(segment) = hop.repeat.as_ref() else {
            unreachable!("segment_rounds serves a step carrying a segment");
        };
        let mut frontier = anchors;
        let mut answer: BTreeMap<RecordId, Value> = BTreeMap::new();
        let mut landing: Vec<RecordRef> = Vec::new();
        for round in 1..=segment.max {
            if frontier.is_empty() {
                break;
            }
            let (found, next, proven) = self
                .one_hop(transaction, origin, &frontier, direction, hop)
                .await?;
            for (child, parent) in proven {
                ledger
                    .parents
                    .entry((child.table, child.id.clone()))
                    .or_insert((parent.table, parent.id.clone()));
            }
            let mut fresh = Vec::new();
            for anchor in next {
                if ledger.seen.insert((anchor.table, anchor.id.clone())) {
                    fresh.push(anchor);
                }
            }
            if round >= segment.min {
                let fresh_ids: std::collections::BTreeSet<RecordId> =
                    fresh.iter().map(|anchor| anchor.id.clone()).collect();
                for (id, value) in found {
                    if fresh_ids.contains(&id) {
                        answer.insert(id, value);
                    }
                }
                landing.extend(fresh.iter().cloned());
            }
            frontier = fresh;
        }
        Ok((answer.into_iter().collect(), landing))
    }
}

/// The statement-wide bookkeeping a segmented walk keeps: the seen set that
/// bounds the work, and the first-reach parent map the `path` route reads
/// back (ADR-0143).
struct WalkLedger {
    seen: BTreeSet<(TableId, RecordId)>,
    parents: BTreeMap<(TableId, RecordId), (TableId, RecordId)>,
}

/// The first-reach chain from a walk's origin to `end`, origin first, built
/// from the parent map the walk recorded (ADR-0143).
fn reach_chain(
    parents: &BTreeMap<(TableId, RecordId), (TableId, RecordId)>,
    end: (TableId, RecordId),
) -> Vec<Value> {
    let mut chain = vec![Value::Record(RecordRef::new(end.0, end.1.clone()))];
    let mut at = end;
    while let Some((parent_table, parent_id)) = parents.get(&at) {
        chain.push(Value::Record(RecordRef::new(
            *parent_table,
            parent_id.clone(),
        )));
        at = (*parent_table, parent_id.clone());
    }
    chain.reverse();
    chain
}

/// Refuse a hop over `table` while its edges are indexed at another moment
/// than the reader sees them.
///
/// A traversal checks its snapshot's position once, at its start; this is the
/// other half, per edge table: a transaction across leaders part-way in it
/// here, whose edges the table's indexes and its readers disagree about
/// (Q-919). A traversal has no scan to fall back to, so either is a refusal.
pub(super) async fn edges_settled(
    transaction: &Transaction<'_>,
    table: TableId,
    named: &tessari_ql::TableRef,
) -> Result<()> {
    if transaction.indexes_are_current_for(table).await? {
        return Ok(());
    }
    let table = named.name.text.clone();
    let span = named.span;
    Err(if transaction.indexes_are_current().await? {
        Error::AcrossSettling { table, span }
    } else {
        behind_the_tail(transaction, table, span)
    })
}

/// The refusal for a walk whose snapshot is behind the tail: a read of the
/// past stays there however often it runs, and a present read another commit
/// passed answers once it runs again (DB-V15).
pub(super) fn behind_the_tail(
    transaction: &Transaction<'_>,
    table: String,
    span: tessari_ql::Span,
) -> Error {
    if transaction.reads_the_past() {
        Error::NoHistoricalTraversal { table, span }
    } else {
        Error::ReadFellBehind { table, span }
    }
}
