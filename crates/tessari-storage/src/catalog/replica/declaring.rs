use super::*;

impl Catalog<'_, '_> {
    /// Declare a peer.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NameTaken`] when the name is already declared.
    pub fn create_replica(
        &mut self,
        name: &str,
        endpoint: &str,
        roles: Roles,
        node: Option<[u8; NODE_ID_LEN]>,
        replicates: Option<Reach>,
        leads: Option<Reach>,
    ) -> Result<ReplicaDefinition> {
        self.create_replica_with(ReplicaDefinition {
            name: name.to_owned(),
            endpoint: endpoint.to_owned(),
            roles,
            node,
            replicates,
            leads,
            clients: None,
            http: None,
            fingerprint: None,
            join: None,
            releasing: false,
            preferred: false,
            region: None,
        })
    }

    /// Declare a peer from a whole definition — the form a declaration with
    /// client addresses (ADR-0101) takes.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NameTaken`] when the name is already declared.
    pub fn create_replica_with(
        &mut self,
        definition: ReplicaDefinition,
    ) -> Result<ReplicaDefinition> {
        if self.replica_row(&definition.name)?.is_some() {
            return Err(Error::NameTaken {
                qualified: qualify(Level::Replica, &[], &definition.name),
            });
        }
        if let Some(node) = definition.node
            && self.is_tombstoned(&node)?
        {
            return Err(Error::NodeTombstoned {
                node: RecordId::Uuid(node).to_string(),
            });
        }
        self.write_replica(&definition);
        Ok(definition)
    }

    /// The stored row a name is written under, read by that key.
    ///
    /// A point read rather than a scan, which is what makes the name an identity
    /// rather than a field somebody searches on: the row either exists at its own
    /// key or it does not, and the answer costs one lookup.
    fn replica_row(&self, name: &str) -> Result<Option<ReplicaDefinition>> {
        let Some(bytes) = self
            .transaction
            .get(&system::address(system::REPLICAS, RecordId::from(name)))?
        else {
            return Ok(None);
        };
        ReplicaDefinition::from_value(&decode_payload(&bytes)?).map(Some)
    }

    /// Write a row at the key its own name gives it.
    ///
    /// Not [`Catalog::write`], which keys by an allocated number. **That number
    /// is what ADR-0077 removed**: it was handed out by the WRITING node while
    /// the table is cluster-wide, so two nodes declaring different peers gave one
    /// number to both and the first replication overwrote one with the other.
    fn write_replica(&mut self, definition: &ReplicaDefinition) {
        self.transaction.put(
            system::address(system::REPLICAS, RecordId::from(definition.name.as_str())),
            encode_payload(&definition.to_value()).into_bytes(),
        );
    }

    /// Bind a declared row to the node whose greeting proved it.
    ///
    /// Writes the `node` field and nothing else. The endpoint, the roles and the
    /// reach stay exactly as the operator declared them, because those are the
    /// operator's decision and the greeting is evidence of an identity only.
    ///
    /// Answers `false` when there is no row under that id — the same shape
    /// [`Self::drop_replica`] uses, and for the same reason: the caller is
    /// reconciling against a list it read a moment ago, and a row that has since
    /// been dropped is an ordinary race rather than a failure.
    ///
    /// It does **not** decide whether the row should be bound. That question has
    /// two refusals in it and they live in [`the_row_a_greeting_binds`], which is
    /// a pure function over the declarations and is therefore testable without a
    /// store.
    ///
    /// # Errors
    ///
    /// Returns an error when the stored definitions cannot be read.
    pub fn bind_replica_node(&mut self, name: &str, node: [u8; NODE_ID_LEN]) -> Result<bool> {
        let Some(mut definition) = self.replica_row(name)? else {
            return Ok(false);
        };
        definition.node = Some(node);
        // The token is spent by the binding it made, in the same write.
        definition.join = None;
        self.write_replica(&definition);
        Ok(true)
    }

    /// Wait on a join token for `name`'s row (`CREATE JOIN TOKEN`).
    ///
    /// Answers `false` when there is no row under that name.
    ///
    /// # Errors
    ///
    /// Returns [`Error::RowAlreadyBound`] when the row already names its node,
    /// and an error when the stored definitions cannot be read.
    pub fn wait_for_join(&mut self, name: &str, ticket: JoinTicket) -> Result<bool> {
        let Some(mut definition) = self.replica_row(name)? else {
            return Ok(false);
        };
        if definition.node.is_some() {
            return Err(Error::RowAlreadyBound {
                name: name.to_owned(),
            });
        }
        definition.join = Some(ticket);
        self.write_replica(&definition);
        Ok(true)
    }

    /// Remove a peer's declaration and release its name.
    ///
    /// Answers `false` when there was nothing under that id.
    ///
    /// Removing the declaration is all this does. The peer is not told, and
    /// nothing chases the data it already holds — which is the honest shape for
    /// a store whose replication is declarative: this statement says *we no
    /// longer count that endpoint as a peer*, and a peer that disagrees is a
    /// question for the operator rather than one a catalog write can settle.
    ///
    /// # The last row that places a range is not dropped
    ///
    /// See [`Self::keeps_a_candidate`]: a range another row still places stays
    /// on its own line, and the last one would leave it two writers.
    ///
    /// # Errors
    ///
    /// Returns [`Error::PlacementCannotBeDropped`] for the last row placing its
    /// range, and an error when the stored definitions cannot be read.
    pub fn drop_replica(&mut self, name: &str) -> Result<bool> {
        let Some(found) = self.replica_row(name)? else {
            return Ok(false);
        };
        self.keeps_a_candidate(&found)?;
        self.transaction
            .delete(system::address(system::REPLICAS, RecordId::from(name)));
        // A row that named a node removes the node, not only the row: its
        // identity and certificate are still valid, and nothing else would
        // keep it from being bound again (ADR-0108 D9).
        if let Some(node) = found.node {
            self.tombstone_node(node);
        }
        Ok(true)
    }

    /// Replace the range a peer's row places (ADR-0098).
    ///
    /// Answers `false` when there is no row under that name. `None` on the
    /// last row placing a range marks it releasing rather than dropping it
    /// (ADR-0098 D3); naming the range a releasing row holds withdraws that.
    ///
    /// # Errors
    ///
    /// [`Error::PlacementCannotBeDropped`] when the last row placing a range
    /// is moved to another one, and an error when the stored definitions
    /// cannot be read.
    pub fn alter_replica_leads(
        &mut self,
        name: &str,
        leads: Option<Reach>,
        preferred: bool,
    ) -> Result<bool> {
        let Some(mut definition) = self.replica_row(name)? else {
            return Ok(false);
        };
        let preferred = preferred && leads.is_some();
        if definition.leads == leads {
            if definition.releasing || definition.preferred != preferred {
                definition.releasing = false;
                definition.preferred = preferred;
                self.write_replica(&definition);
            }
            return Ok(true);
        }
        // The last row placing a range gives it back to the store line rather
        // than dropping it (ADR-0098 D3): the range stays carved while the
        // store's leader is elected on its line, and is folded away then.
        if leads.is_none()
            && definition.leads.is_some()
            && !self.has_another_candidate(&definition)?
        {
            definition.releasing = true;
            definition.preferred = false;
            self.write_replica(&definition);
            return Ok(true);
        }
        self.keeps_a_candidate(&definition)?;
        definition.leads = leads;
        definition.releasing = false;
        definition.preferred = preferred;
        self.write_replica(&definition);
        Ok(true)
    }

    /// Fold a released placement away, once the store line's leader leads
    /// the range as well (ADR-0098 D3): the range returns to the store line,
    /// which the same node leads, so no instant has two writers.
    ///
    /// Answers `false` when no row under that name is releasing — the hand-back
    /// was withdrawn, or already folded.
    ///
    /// # Errors
    ///
    /// The store's, reading or decoding the row.
    pub fn finish_release(&mut self, name: &str) -> Result<bool> {
        let Some(mut definition) = self.replica_row(name)? else {
            return Ok(false);
        };
        if !definition.releasing {
            return Ok(false);
        }
        definition.leads = None;
        definition.releasing = false;
        definition.preferred = false;
        self.write_replica(&definition);
        Ok(true)
    }

    /// Amend the row named `name` in place with `amend`, and answer whether
    /// there was one (Q-892).
    ///
    /// For the clauses no other row depends on — where the peer answers, what
    /// it is for, where clients and HTTP reach it. The placement has a rule of
    /// its own and goes through [`Self::alter_replica_leads`]. The node the row
    /// is bound to, its subscription and its fingerprint are not offered to
    /// `amend` by any caller, so a row keeps the identity it was bound to.
    ///
    /// # Errors
    ///
    /// The store's, reading or decoding the row.
    pub fn amend_replica(
        &mut self,
        name: &str,
        amend: impl FnOnce(&mut ReplicaDefinition),
    ) -> Result<bool> {
        let Some(mut definition) = self.replica_row(name)? else {
            return Ok(false);
        };
        amend(&mut definition);
        self.write_replica(&definition);
        Ok(true)
    }

    /// Refuses taking `row`'s placement when no other row places its range.
    ///
    /// Another candidate keeps the range on its own line, whose election
    /// already decides between two nodes. The last one would hand the range
    /// back to the store line at once on the node that committed the change,
    /// while the range's own leader goes on writing under its lease until the
    /// change reaches it — two writers on one range for up to a lease
    /// (ADR-0082, ADR-0098).
    fn keeps_a_candidate(&self, row: &ReplicaDefinition) -> Result<()> {
        if row.leads.is_none() || self.has_another_candidate(row)? {
            return Ok(());
        }
        Err(Error::PlacementCannotBeDropped {
            name: row.name.clone(),
        })
    }

    /// Whether a row other than `row` places `row`'s range.
    fn has_another_candidate(&self, row: &ReplicaDefinition) -> Result<bool> {
        Ok(self
            .replicas()?
            .iter()
            .any(|peer| peer.name != row.name && peer.leads.is_some() && peer.leads == row.leads))
    }

    /// Every declared peer, in name order.
    ///
    /// Sorted here rather than by the caller, for the reason `INFO FOR`'s name
    /// lists are sorted: the catalog hands these back in the order somebody
    /// happened to declare them, and an answer whose shape depends on that is
    /// two answers to one question — which is worse here than elsewhere,
    /// because two nodes comparing peer lists is the point of having one.
    ///
    /// # Errors
    ///
    /// Returns an error when a stored definition cannot be read.
    pub fn replicas(&self) -> Result<Vec<ReplicaDefinition>> {
        let mut found = Vec::new();
        for (_, payload) in self.transaction.scan_table(
            system::SYSTEM_NAMESPACE,
            system::SYSTEM_DATABASE,
            system::REPLICAS,
        )? {
            found.push(ReplicaDefinition::from_value(&decode_payload(&payload)?)?);
        }
        found.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(found)
    }

    /// What the cluster says a node should be, if anything says so.
    ///
    /// The **desired** role of `04_concept.md` §6.1: a replicated catalog record
    /// an operator writes, against which a node reconciles what it actually
    /// holds. `None` when no membership row names this node — which is every
    /// store until somebody binds one, and is why this changes nothing for a
    /// node standing on its own.
    ///
    /// # One place, so the two readers cannot disagree
    ///
    /// Two callers ask this question — the node reconciling itself at open, and
    /// `INFO FOR NODE` reporting what it will reconcile to — and they must never
    /// answer it differently, because the whole value of reporting a desired
    /// role is that it predicts the one that will be adopted. So the rule for
    /// *which row is mine* lives here and is called twice, rather than being
    /// written twice and kept in step by hand.
    ///
    /// # The first match, and why there can only be one
    ///
    /// Nothing stops an operator binding two rows to one node, and nothing here
    /// tries to arbitrate: `replicas` hands them back in **name order**, so the
    /// answer is stable rather than dependent on declaration order, which is the
    /// property that matters when two nodes compare what they think the cluster
    /// says. A second binding is an operator error and is visible in
    /// `INFO FOR NODE`'s peer list, where both rows are shown carrying the same
    /// id.
    ///
    /// # Errors
    ///
    /// Returns an error when a stored definition cannot be read.
    pub fn desired_roles(&self, node: &[u8; NODE_ID_LEN]) -> Result<Option<Roles>> {
        Ok(self
            .replicas()?
            .into_iter()
            .find(|found| found.node.as_ref() == Some(node))
            .map(|found| found.roles))
    }
}
