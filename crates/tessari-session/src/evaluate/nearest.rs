//! A filtered nearest-neighbour read served by the vector graph.
//!
//! `WHERE … ORDER BY vector::cosine(f, $q) LIMIT k APPROXIMATE` used to fall to
//! the exact scan the moment a condition appeared: the graph was only asked for
//! a read with no `WHERE`. The walk here navigates the whole graph and admits a
//! record into the answer only once it has been read at this reader's snapshot,
//! redacted for this session, and tested against the **whole** condition — the
//! same three steps every index-served read takes, so the graph can choose which
//! records are tried and never which records pass.
//!
//! # When the walk gives the read back
//!
//! A walk cut at its ceiling, or one that admitted fewer records than the bound
//! asks for, answers nothing: the read goes to the exact path with a
//! `FellBack` note. `APPROXIMATE` agreed to a graph's choice among the nearest,
//! not to a short page — and the exact read also fills the bound with records
//! that hold no vector, which a graph cannot reach.
//!
//! # A small candidate set is answered exactly
//!
//! When an index on the condition has already narrowed the read to no more
//! records than the walk would visit, distances to every candidate cost no more
//! than the walk and give the exact answer, so the existing exact path keeps
//! them (see `Prepared::Filtered` in `produce.rs`).

use std::collections::BTreeMap;

use tessari_storage::{Admit, RecordAddress, Transaction};
use tessari_types::{RecordId, TableId, Value};

use crate::condition::boolean;
use crate::error::{Error, Result};
use crate::plan;
use crate::redact::Visible;
use crate::session::Session;

use super::{Scope, Testing, Walked};

impl Session<'_> {
    /// Walk the vector graph for the records nearest the query that the
    /// condition admits.
    ///
    /// [`Walked::NotServed`] when no graph may serve the read (the refusals in
    /// [`Session::nearest_gate`]); [`Walked::Declined`] when one tried and came
    /// back cut or short, so the exact path answers with a note saying so.
    pub(super) async fn walk_admitted(
        &self,
        transaction: &mut Transaction<'_>,
        context: crate::context::Context,
        table: TableId,
        wanted: &plan::Nearest<'_>,
        testing: Testing<'_>,
    ) -> Result<Walked> {
        let Some(gated) = self
            .nearest_gate(transaction, context, table, wanted)
            .await?
        else {
            return Ok(Walked::NotServed);
        };
        // The partitioned kind probes where the graph walks; the contract —
        // laziness, ceiling, a short or cut answer declined to the exact path —
        // is the same either way. The probe handle is taken before the
        // admitting borrows the transaction, as the graph was.
        enum Source {
            Probe(tessari_storage::VectorPartitioned),
            Graph(tessari_storage::VectorGraph),
        }
        let source = if gated.index.partitions.is_some() {
            let Some(probe) = transaction.vector_partitioned(&gated.index).await? else {
                return Ok(Walked::NotServed);
            };
            Source::Probe(probe)
        } else {
            let Some(graph) = transaction.vector_graph(&gated.index)? else {
                return Ok(Walked::NotServed);
            };
            Source::Graph(graph)
        };
        let mut admitting = Admitting {
            session: self,
            transaction,
            place: (context, table),
            visible: &gated.visible,
            testing,
            admitted: BTreeMap::new(),
        };
        let window = plan::walked_for(
            wanted.wanted,
            gated.index.quantized || gated.index.partitions.is_some(),
        );
        let matched = match &source {
            Source::Probe(probe) => {
                probe
                    .nearest_matching(&gated.query, window, wanted.effort, &mut admitting)
                    .await?
            }
            Source::Graph(graph) => {
                graph
                    .nearest_matching(&gated.query, window, wanted.effort, &mut admitting)
                    .await?
            }
        };
        let mut admitted = admitting.admitted;
        if matched.cut || matched.ids.len() < wanted.wanted {
            return Ok(Walked::Declined);
        }
        let found = matched
            .ids
            .into_iter()
            .filter_map(|id| admitted.remove(&id).map(|record| (id, record)))
            .collect();
        Ok(Walked::Served {
            found,
            index: gated.index.name,
        })
    }
}

/// The condition a filtered walk admits by: a record read at this reader's own
/// snapshot, redacted for this session and tested against the whole condition.
struct Admitting<'w, 's, 't> {
    session: &'w Session<'s>,
    transaction: &'w mut Transaction<'t>,
    place: (crate::context::Context, TableId),
    visible: &'w Visible,
    testing: Testing<'w>,
    /// The admitted records, kept so the answer is not read twice.
    admitted: BTreeMap<RecordId, Value>,
}

impl Admit for Admitting<'_, '_, '_> {
    type Error = Error;

    async fn admit(&mut self, id: &RecordId) -> Result<bool> {
        let (context, table) = self.place;
        // Read at this reader's own snapshot: a node left behind by a deleted
        // record has nothing to read and is not admitted.
        let at = RecordAddress::new(context.namespace, context.database, table, id.clone());
        let Some(payload) = self.transaction.get(&at).await? else {
            return Ok(false);
        };
        let record = self.session.record_of(&payload, self.visible)?;
        // Row guards (ADR-0140): a candidate the guards do not reach is not
        // admitted, the same verdict a failed condition gives it.
        let rows = self.session.rows_in(self.transaction, table).await?;
        if self
            .session
            .permitted(self.transaction, id, record.clone(), self.visible, &rows)
            .await?
            .is_none()
        {
            return Ok(false);
        }
        let (condition, searched, noticed) = (
            self.testing.condition,
            self.testing.searched,
            self.testing.noticed,
        );
        let held = self
            .session
            .evaluate_in(
                self.transaction,
                condition,
                Scope::searching(&record, searched)
                    .identified(id)
                    .noticing(noticed),
            )
            .await?;
        if !boolean(&held, condition.span)? {
            return Ok(false);
        }
        self.admitted.insert(id.clone(), record);
        Ok(true)
    }
}
