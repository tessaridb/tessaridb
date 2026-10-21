//! The commit of a transaction that writes only spaces held in memory
//! (ADR-0130).
//!
//! It is the logged commit with the log left out: the same turn at the write
//! gate, the same conflict check, schema, limit and derivation, at the next
//! store version — and no log entry, no position, nothing a follower collects.
//! What lands, lands in node memory through the routed backend.

use std::collections::BTreeSet;

use super::*;
use tessari_types::DatabaseId;

use crate::catalog::{Catalog, SpaceDeclaration, TableKind};

/// The held spaces a transaction writes, by place, and the first one's name
/// for a refusal.
pub(super) struct Held {
    space: String,
    places: Vec<(NamespaceId, DatabaseId, TableId)>,
}

impl Transaction<'_> {
    /// The held space this transaction writes, when every record it writes is
    /// one — or `None` when it writes none.
    ///
    /// # Errors
    ///
    /// [`Error::InMemoryBesideDurable`] when it writes a held space and
    /// anything else, and a substrate failure reading the catalog.
    pub(super) async fn held_only(&mut self) -> Result<Option<Held>> {
        let places: BTreeSet<(NamespaceId, DatabaseId, TableId)> = self
            .writes
            .keys()
            .map(|address| (address.namespace, address.database, address.table))
            .collect();
        let mut held: Option<Held> = None;
        let mut durable = false;
        for (namespace, database, table) in places {
            let definition = Catalog::new(self).table_shared(table).await?;
            match definition.as_deref() {
                Some(definition)
                    if definition.namespace == namespace
                        && definition.database == database
                        && matches!(
                            definition.kind,
                            TableKind::Space(SpaceDeclaration { memory: true, .. })
                        ) =>
                {
                    let held = held.get_or_insert_with(|| Held {
                        space: definition.name.clone(),
                        places: Vec::new(),
                    });
                    held.places.push((namespace, database, table));
                }
                _ => durable = true,
            }
        }
        match held {
            Some(held) if durable => Err(Error::InMemoryBesideDurable { space: held.space }),
            held => Ok(held),
        }
    }

    /// Commit (or rehearse) a transaction whose writes are all held records.
    pub(super) async fn settle_held(self, settle: Settle, held: Held) -> Result<Committed> {
        let identity = self.store.node_identity().await?;
        if self.store.in_a_cluster(&identity.id).await? {
            return Err(Error::InMemoryOnACluster { space: held.space });
        }
        let log = self
            .store
            .own_log(crate::store::UNPARTITIONED_REPORT_HOME)
            .await?;
        let placement = self.placement().await?;
        let mut record = self.log_record(identity.id, &placement).await?;
        let mut attempt = 0_u32;
        loop {
            attempt = attempt.saturating_add(1);
            if attempt > MAX_COMMIT_ATTEMPTS {
                return Err(Error::CommitContention {
                    attempts: MAX_COMMIT_ATTEMPTS,
                });
            }
            let turn = self.store.write_gate().hold().await;
            self.check_for_conflicts().await?;
            crate::schema::validate(self.store, &record).await?;
            let mut evicted = crate::bounded::enforce(self.store, &record, identity.id).await?;
            if let Some(admitted) = crate::lifetime::admit(
                self.store,
                evicted.as_ref().unwrap_or(&record),
                self.clock(),
                &self.lifetimes,
            )
            .await?
            {
                evicted = Some(admitted);
            }
            let carried = match evicted.as_mut() {
                Some(carrying) => carrying,
                None => &mut record,
            };
            let commit_version = Sequence::new(
                self.store
                    .committed_version()
                    .await?
                    .get()
                    .saturating_add(1),
            );
            carried.set_order(commit_version);
            let written = crate::log::state_batch(commit_version, carried);
            let batch = self.derived(carried, written, commit_version).await?;
            if matches!(settle, Settle::Discard) {
                return Ok(Committed {
                    log,
                    sequence: self.store.committed_tail(log).await?,
                });
            }
            // Before the batch is built would be the earliest, and this is it:
            // from here every key of the space is routed to memory, so none of
            // this batch reaches the store's backend.
            for (namespace, database, table) in &held.places {
                self.store.held_spaces().hold(*namespace, *database, *table);
            }
            let backend = self.store.engine().as_ref();
            let landed = if backend.groups_writes() {
                let ticket = self.store.write_gate().stage(batch, commit_version);
                drop(turn);
                self.store.write_gate().land(ticket, backend).await
            } else {
                let applied = self
                    .store
                    .write_gate()
                    .apply(batch, backend, crate::gate::Landing::Synced)
                    .await;
                drop(turn);
                applied
            };
            match landed {
                Ok(()) => {
                    self.store.landed_at(commit_version);
                    return Ok(Committed {
                        log,
                        sequence: self.store.committed_tail(log).await?,
                    });
                }
                Err(tessari_kv::Error::Conflict { .. }) => {
                    back_off(attempt).await;
                }
                Err(other) => return Err(other.into()),
            }
        }
    }
}
