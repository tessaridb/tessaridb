//! Declaring namespaces, databases and tables, and a tenancy's replication.

use super::system::Level;
use super::{
    Catalog, DatabaseDefinition, EDGE_IN, EDGE_OUT, FieldShape, IndexShape, NamespaceDefinition,
    TableDefinition, TableKind, TableShape, qualify, shard, system,
};
use crate::error::{Error, Result};
use tessari_types::{
    Acknowledgement, DatabaseId, FieldKind, NamespaceId, Path, Replication, ReplicationClass,
    TableId,
};

impl<'a, 'txn> Catalog<'a, 'txn> {
    /// Create a namespace.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NameTaken`] when the name is already in use, and a
    /// substrate or decoding failure otherwise.
    pub fn create_namespace(&mut self, name: &str) -> Result<NamespaceDefinition> {
        let qualified = qualify(Level::Namespace, &[], name);
        self.reserve_name(&qualified)?;
        let id = NamespaceId::new(self.allocate(Level::Namespace)?);
        let definition = NamespaceDefinition {
            id,
            name: name.to_owned(),
            // A namespace is created having said nothing about replication, and
            // the clause is applied by [`Self::set_replication`] whether it
            // arrived with the `DEFINE` or with a later `ALTER`. One write path
            // rather than two: the two statements set the same field, and a
            // second route for the creating case is a route that can disagree
            // with the altering one. Both run inside the caller's transaction,
            // whose pending writes are keyed by address, so a definition
            // written and then amended still reaches the log as one mutation.
            replication: None,
            // The same, for the same reason: applied by
            // [`Self::set_replication_class`] whichever statement carried it.
            class: None,
            // And again: [`Self::set_acknowledgement`].
            acknowledge: None,
        };
        self.write(system::NAMESPACES, id.get(), &definition.to_value());
        self.claim_name(&qualified, id.get());
        Ok(definition)
    }

    /// Set how many copies of a namespace the cluster is asked to keep.
    ///
    /// Moves between **stated** values in both directions (owner requirement
    /// D12) and never back to never-stated: a namespace that was once asked has
    /// been asked, and silence is a fact about its history rather than a
    /// setting to restore.
    ///
    /// Nothing is redistributed here, and nothing needs to be. The log already
    /// holds every write the namespace ever took, so a follower that begins
    /// subscribing replays it, and this statement has nothing to do but record
    /// the policy. See `Session::alter_namespace` for why that is a property of
    /// the design rather than a step left out.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NoSuchParent`] when the namespace does not exist, and a
    /// substrate or decoding failure otherwise.
    pub fn set_replication(
        &mut self,
        namespace: NamespaceId,
        replication: Replication,
    ) -> Result<NamespaceDefinition> {
        let Some(mut definition) = self.namespace(namespace)? else {
            return Err(Error::NoSuchParent {
                entity: "namespace",
                id: namespace.get(),
            });
        };
        definition.replication = Some(replication);
        self.write(system::NAMESPACES, namespace.get(), &definition.to_value());
        Ok(definition)
    }

    /// Set how many writers a namespace admits (G027 S2.1).
    ///
    /// The sibling of [`Self::set_replication`] and deliberately the same shape,
    /// so the class cannot acquire a second write path the count does not have.
    /// It moves between **stated** values and never back to never-stated, for
    /// that method's reason: a namespace that was once asked has been asked.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NoSuchParent`] when the namespace does not exist, and
    /// the substrate or decoding failure otherwise.
    pub fn set_replication_class(
        &mut self,
        namespace: NamespaceId,
        class: ReplicationClass,
    ) -> Result<NamespaceDefinition> {
        let Some(mut definition) = self.namespace(namespace)? else {
            return Err(Error::NoSuchParent {
                entity: "namespace",
                id: namespace.get(),
            });
        };
        definition.class = Some(class);
        self.write(system::NAMESPACES, namespace.get(), &definition.to_value());
        Ok(definition)
    }

    /// Set how many copies must hold a write in a namespace before it is
    /// acknowledged, and whether a request may ask for fewer (ADR-0106 D2).
    ///
    /// # Errors
    ///
    /// Returns [`Error::NoSuchParent`] when the namespace does not exist.
    pub fn set_acknowledgement(
        &mut self,
        namespace: NamespaceId,
        acknowledge: Acknowledgement,
    ) -> Result<NamespaceDefinition> {
        let Some(mut definition) = self.namespace(namespace)? else {
            return Err(Error::NoSuchParent {
                entity: "namespace",
                id: namespace.get(),
            });
        };
        definition.acknowledge = Some(acknowledge);
        self.write(system::NAMESPACES, namespace.get(), &definition.to_value());
        Ok(definition)
    }

    /// Create a database inside an existing namespace.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NoSuchParent`] when the namespace does not exist,
    /// [`Error::NameTaken`] when the name is in use within it.
    pub fn create_database(
        &mut self,
        namespace: NamespaceId,
        name: &str,
    ) -> Result<DatabaseDefinition> {
        if self.namespace(namespace)?.is_none() {
            return Err(Error::NoSuchParent {
                entity: "namespace",
                id: namespace.get(),
            });
        }
        let qualified = qualify(Level::Database, &[namespace.get()], name);
        self.reserve_name(&qualified)?;
        let id = DatabaseId::new(self.allocate(Level::Database)?);
        let definition = DatabaseDefinition {
            id,
            namespace,
            name: name.to_owned(),
        };
        self.write(system::DATABASES, id.get(), &definition.to_value());
        self.claim_name(&qualified, id.get());
        Ok(definition)
    }

    /// Create a table inside an existing database.
    ///
    /// The shape is fixed at creation except for `schemafull`, which
    /// [`Self::set_schemafull`] rewrites in place. That one moves because a
    /// schema is a rule about what may be *written*, so changing it binds the
    /// writes that follow and leaves the stored rows alone; the `kind` does not
    /// move, because it describes what the records already **are**.
    ///
    /// An edge table additionally gets an index on `out` and one on `in`, in
    /// this same commit, so that traversal is an index read without the caller
    /// having had to know to declare them.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NoSuchParent`] when the database does not exist or does
    /// not belong to `namespace`, and [`Error::NameTaken`] when the name is in
    /// use within it.
    pub fn create_table(
        &mut self,
        namespace: NamespaceId,
        database: DatabaseId,
        name: &str,
        shape: TableShape,
    ) -> Result<TableDefinition> {
        let parent = self.database(database)?;
        if parent.is_none_or(|found| found.namespace != namespace) {
            return Err(Error::NoSuchParent {
                entity: "database",
                id: database.get(),
            });
        }
        let shards = shard::declared_for(name, &shape)?;
        // The store names a partitioned record as the field's value and a UUID
        // v7 after it (ADR-0096), so a counter has nowhere to go.
        if shape.partition.is_some() && shape.identity != tessari_types::IdentityKind::Uuid {
            return Err(Error::PartitionNeedsGeneratedUuid {
                table: name.to_owned(),
            });
        }
        let qualified = qualify(Level::Table, &[namespace.get(), database.get()], name);
        self.reserve_name(&qualified)?;
        let id = TableId::new(self.allocate(Level::Table)?);
        let definition = TableDefinition {
            id,
            namespace,
            database,
            name: name.to_owned(),
            schemafull: shape.schemafull,
            kind: shape.kind,
            identity: shape.identity,
            graph: shape.graph,
            conflict: shape.conflict,
            shards,
            partition: shape.partition,
            events: Vec::new(),
        };
        self.write(system::TABLES, id.get(), &definition.to_value());
        self.claim_name(&qualified, id.get());
        // Learned here rather than on the first read, so a table declared in
        // this process never costs a catalog round trip to recognise. Recorded
        // before the commit, which is deliberate and harmless: a rolled-back
        // creation leaves an entry for a table id nothing can address, and ids
        // are never reused.
        self.transaction
            .store()
            .series()
            .learn(id, &definition.kind);
        // Learned here for the same reason, and because the commit that writes
        // this table's first records may be this very transaction: the map it
        // stamps them with must not have to come from a catalog read that cannot
        // yet see the declaration.
        self.transaction
            .store()
            .shards()
            .learn(id, definition.shards.as_ref());
        if matches!(definition.kind, TableKind::Edge(_)) {
            // Every edge table gets the endpoint machinery, declared pair or
            // not: what the pair adds is a refusal at the write and an order on
            // the key, not a different way of being reachable.
            //
            // Each endpoint gets both an index and a declaration. The index is
            // what makes traversal a range read; the declaration is what lets an
            // edge table also be `SCHEMAFULL`, since nobody writes `out` and `in`
            // by hand and a caller should not have to declare fields the store
            // itself fills in.
            for endpoint in [EDGE_OUT, EDGE_IN] {
                self.create_index(
                    id,
                    &format!("{endpoint}_edges"),
                    vec![Path::field(endpoint)],
                    IndexShape::default(),
                )?;
                self.create_field(id, endpoint, FieldKind::Record, FieldShape::default())?;
            }
        }
        if matches!(definition.kind, TableKind::Bucket(_)) {
            // The companion table the bytes live in. Its name carries a byte an
            // identifier cannot hold, so no statement can name it — the same
            // mechanism the catalog itself uses to be unreachable rather than
            // merely undocumented, and the reason `SELECT * FROM media` answers
            // with files and never with chunks (ADR-0011 §2).
            self.create_table(
                namespace,
                database,
                &Self::chunks_named(name),
                TableShape::default(),
            )?;
        }
        Ok(definition)
    }

    /// The name of the table a bucket's chunks live in.
    ///
    /// Derived rather than stored: the name carries the fact, so a second field
    /// in the catalog holding the same id would be a fact that can disagree with
    /// itself. The `\u{1}` is what makes it unnameable — an identifier is
    /// letters, digits and underscores, so nothing a caller can write reaches it.
    #[must_use]
    pub fn chunks_named(bucket: &str) -> String {
        format!("{bucket}\u{1}chunks")
    }

    /// The name of the table an edge kind's edges live in.
    ///
    /// Derived rather than stored, and unnameable for the same reason a bucket's
    /// chunk table is: an identifier is letters, digits and underscores, so the
    /// `\u{1}` puts it out of reach of anything a caller can write. An edge kind
    /// is not a table in the language, and this is what keeps that true while
    /// still letting an edge be an ordinary record mutation — which is what
    /// carries it, and the adjacency derived from it, to every replica.
    #[must_use]
    pub fn edges_named(kind: &str) -> String {
        format!("{kind}\u{1}edges")
    }
}
