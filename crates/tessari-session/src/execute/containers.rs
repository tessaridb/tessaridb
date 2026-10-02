//! Declaring and dropping namespaces, databases, graphs and edge kinds.

mod graphs;
use tessari_ql::{Name, NamespaceChange, Span};
use tessari_storage::{Catalog, Transaction};

use tessari_types::{Acknowledgement, Replication, ReplicationClass};

use crate::error::{Depended, Error, Result};
use crate::outcome::Outcome;
use crate::session::Session;

impl Session<'_> {
    pub(super) fn define_namespace(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        if_not_exists: bool,
        (replication, class, acknowledge): (
            Option<Replication>,
            Option<ReplicationClass>,
            Option<Acknowledgement>,
        ),
    ) -> Result<Outcome> {
        if if_not_exists
            && Catalog::new(transaction)
                .namespace_id(&name.text)?
                .is_some()
        {
            // The clause is not applied on this branch, and that is the same
            // reading `IF NOT EXISTS` already has everywhere else: the
            // statement did nothing because the namespace was there, so it
            // changes nothing about it either. A definition that quietly
            // re-set a policy on a namespace it did not create would be an
            // `ALTER` wearing a `DEFINE`'s spelling.
            return Ok(Outcome::Done);
        }
        // Asked only of the branch that actually creates one, and only when the
        // statement said nothing: a store with no peers has nowhere to put a
        // second copy, so there the bare form is what a single-node install has
        // always written and is stored as *never stated*. A store that declares
        // a peer is a cluster, and there a namespace holding one copy is a
        // decision somebody is making — ADR-0060's whole point — so it is
        // written down rather than inherited.
        if replication.is_none() {
            let peers = Catalog::new(transaction).replicas()?.len();
            if peers > 0 {
                return Err(Error::ReplicationUnstated {
                    namespace: name.text.clone(),
                    peers,
                    span: name.span,
                });
            }
        }
        let definition = Catalog::new(transaction).create_namespace(&name.text)?;
        if let Some(replication) = replication {
            // Through the same call an `ALTER` makes, so the two statements
            // cannot set this field differently.
            Catalog::new(transaction).set_replication(definition.id, replication)?;
        }
        if let Some(class) = class {
            // The same route for the same reason. No `ALTER` sets the class
            // today — G027 S2.1 needs only a declaration — and the setter
            // exists in the shape an `ALTER` would use so that adding one later
            // is a statement rather than a second write path.
            Catalog::new(transaction).set_replication_class(definition.id, class)?;
        }
        if let Some(acknowledge) = acknowledge {
            // Through the setter an `ALTER NAMESPACE … ACKNOWLEDGE` uses.
            Catalog::new(transaction).set_acknowledgement(definition.id, acknowledge)?;
        }
        Ok(Outcome::Done)
    }

    /// `ALTER NAMESPACE prod REPLICATION FACTOR 3`
    ///
    /// Turning replication on for a namespace that already holds data, and off
    /// again (owner requirement D12). **Nothing is redistributed**, and the
    /// absence of a repair step is the point rather than an omission: the log
    /// already holds every write the namespace ever took, so a follower that
    /// begins subscribing replays it from origin. Cassandra's `ALTER KEYSPACE`
    /// needs a `nodetool repair` afterwards because its replicas hold data
    /// rather than a history; ours needs none because the history is the store.
    pub(super) fn alter_namespace(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        change: NamespaceChange,
    ) -> Result<Outcome> {
        let Some(namespace) = Catalog::new(transaction).namespace_id(&name.text)? else {
            return Err(Error::Unknown {
                entity: "namespace",
                name: name.text.clone(),
                span: name.span,
            });
        };
        match change {
            NamespaceChange::Replication(replication) => {
                Catalog::new(transaction).set_replication(namespace, replication)?;
            }
            NamespaceChange::Acknowledge(acknowledge) => {
                Catalog::new(transaction).set_acknowledgement(namespace, acknowledge)?;
            }
        }
        Ok(Outcome::Done)
    }

    pub(super) fn define_database(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        if_not_exists: bool,
        span: Span,
    ) -> Result<Outcome> {
        let namespace = self.namespace_id(transaction, span)?;
        if if_not_exists
            && Catalog::new(transaction)
                .database_id(namespace, &name.text)?
                .is_some()
        {
            return Ok(Outcome::Done);
        }
        Catalog::new(transaction).create_database(namespace, &name.text)?;
        Ok(Outcome::Done)
    }

    /// `DROP DATABASE staging` — refused while it still holds a table.
    ///
    /// The bound is the one `DELETE … LIMIT` established: a destructive
    /// statement carrying no predicate at all is the widest thing this language
    /// can be asked to run, and the person writing it is thinking about one
    /// name. The refusal counts and names, so acting on it needs no second
    /// query.
    pub(super) fn drop_database(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        span: Span,
    ) -> Result<Outcome> {
        let context = self.context(transaction, Some(name.text.as_str()), span)?;
        let held = Catalog::new(transaction).tables_in(context.namespace, context.database)?;
        if let Some(first) = held.first() {
            return Err(Error::StillDepended {
                depended: Depended::DatabaseByTable,
                name: name.text.clone(),
                count: held.len(),
                first: first.name.clone(),
                span,
            });
        }
        Catalog::new(transaction).drop_database(context.database)?;
        Ok(Outcome::Done)
    }

    /// `DROP NAMESPACE acme` — refused while it still holds a database.
    ///
    /// One level up from [`Self::drop_database`] and refusing on the same
    /// ground. Resolved by name against the catalog rather than through the
    /// session's tenancy, because a namespace is what a tenancy is selected
    /// *within* — asking the context for it would require having selected it.
    pub(super) fn drop_namespace(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        span: Span,
    ) -> Result<Outcome> {
        let id = Catalog::new(transaction)
            .namespace_id(&name.text)?
            .ok_or_else(|| Error::Unknown {
                entity: "namespace",
                name: name.text.clone(),
                span,
            })?;
        let held = Catalog::new(transaction).databases_in(id)?;
        if let Some(first) = held.first() {
            return Err(Error::StillDepended {
                depended: Depended::NamespaceByDatabase,
                name: name.text.clone(),
                count: held.len(),
                first: first.name.clone(),
                span,
            });
        }
        Catalog::new(transaction).drop_namespace(id)?;
        Ok(Outcome::Done)
    }
}
