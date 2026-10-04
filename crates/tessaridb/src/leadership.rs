//! Leadership as this database sees it: the lease it holds, who leads a range, and
//! which peer may take a write.

use super::*;

impl Db {
    /// Take or renew the lease this node writes under.
    ///
    /// The seam between the cluster and the engine: a candidate that carried a
    /// round to a majority of voting members tells the store how long that
    /// majority agreed it may write for, and the store closes its fence
    /// `LEASE_GUARD` before the span runs out. Nothing here asks who granted it
    /// — the round did that, and a store that re-checked would be checking a
    /// fact it has no way to know.
    ///
    /// A node nobody granted leadership to never calls this and is not fenced:
    /// it is not a leader running out of time.
    pub fn hold_lease(&self, ttl: core::time::Duration) {
        self.store.hold_lease(ttl);
    }

    /// Hold a lease a majority granted, exactly as it was granted.
    ///
    /// The form a cluster uses, and the difference from [`Db::hold_lease`] is
    /// the instant. A granted lease is dated from when its round **opened**, so
    /// a slow round yields a shorter window; a span arriving here instead would
    /// restart that clock on installation and spend the collection delay out of
    /// the voters' window rather than this node's.
    ///
    /// A node nobody granted leadership to never calls either form and is not
    /// fenced by one.
    ///
    /// The epoch travels with the lease because they were granted together and
    /// are read together: the fence answers *may I still write*, the epoch
    /// answers *which leadership am I writing under*, and a greeting carries the
    /// second to every peer that routes on it.
    pub fn hold(&self, epoch: tessari_types::Epoch, lease: Lease) {
        self.store.hold(epoch, lease);
    }

    /// Write into the log that this node took the leadership of `range`.
    ///
    /// The companion of [`Db::hold`] and deliberately **not** part of it. `hold`
    /// installs a grant a majority already made and cannot fail; this records
    /// that grant in the log, and can. Folding the two together would let a
    /// storage hiccup revoke a decision the cluster had taken — a node that won
    /// a round and could not write the row still legitimately holds the lease,
    /// because the row is a *record* of the grant and never the grant itself.
    ///
    /// # Called on a change and never on a renewal
    ///
    /// A lease is renewed for as long as a node keeps leading. Writing this per
    /// renewal would put a log record on every round forever, which every
    /// follower then pays to apply. The caller writes it only when the epoch it
    /// holds is not the one it held a moment ago.
    ///
    /// # What this buys
    ///
    /// It is the only thing that makes *who leads this range* answerable from
    /// the log. A [`tessari_encoding::LogRecord`] carries the epoch it was
    /// written under but never the node that wrote it, so until this row exists
    /// the answer can only come from a greeting — over the network, from a peer
    /// that has to be reachable.
    ///
    /// # It writes this node, and cannot be asked to write another
    ///
    /// There is no `node` argument. The id comes from this store's own identity,
    /// which is deliberately the one thing that never travels in the log
    /// (ADR-0018) — so *this node took a leadership* is the only sentence this
    /// method can produce. A signature taking the id would have been a way for
    /// any caller to write a row claiming somebody **else** leads, which every
    /// other node would then apply and route on.
    ///
    /// # Errors
    ///
    /// Returns an error when this node's identity or the transaction cannot be
    /// read, the row cannot be encoded, or the commit is refused — including by
    /// this node's own lease fence, which is the correct refusal: a node past
    /// its fence may not write.
    pub fn record_leadership(&self, range: Reach, epoch: tessari_types::Epoch) -> Result<()> {
        let me = self.store.node_identity()?.id;
        let mut transaction = self.store.begin()?;
        Catalog::new(&mut transaction).record_leadership(range, me, epoch)?;
        transaction.commit()?;
        Ok(())
    }

    /// Who the log says leads `range`, and under which leadership.
    ///
    /// Answered from a row this node holds because it **applied the log record
    /// that created it** — no greeting, no peer, no socket. A node cut off from
    /// every other still answers, from what it had already applied.
    ///
    /// `None` means the log has never carried a leadership covering that range,
    /// which is the honest answer for a store that has elected nobody.
    ///
    /// The answer carries the epoch it was decided under, so a caller holding a
    /// newer one knows this describes an arrangement that has been superseded
    /// rather than following it.
    ///
    /// # Errors
    ///
    /// Returns an error when the transaction cannot be opened or a stored
    /// definition cannot be read.
    pub fn leader_of(&self, range: Reach) -> Result<Option<LeadershipDefinition>> {
        let mut transaction = self.store.begin()?;
        Ok(Catalog::new(&mut transaction).leader_of(range)?)
    }

    /// The leadership epoch this node is writing under, if a round granted it
    /// one.
    #[must_use]
    pub fn leading(&self) -> Option<tessari_types::Epoch> {
        self.store.leading()
    }

    /// Where the peer that takes writes answers, if one is declared.
    ///
    /// The forward's target (ADR-0019 §2, case *forward*). At v1 there is one
    /// range covering everything, so "the leader of the range this statement
    /// touches" and "the peer declared writable" are the same peer — which is
    /// what ADR-0019 §1 means by the same lookup serving both eras, and why this
    /// answers with the endpoint rather than with a range.
    ///
    /// `None` when nothing is declared writable. That is an answer, not a
    /// failure: a node that may not write and knows of nobody who may is
    /// correctly configured for a cluster of one that has been drained, and the
    /// caller says so rather than guessing at an address.
    ///
    /// **More than one is refused.** Two peers declared writable is the split
    /// brain the whole design is arranged to prevent, and picking either one —
    /// the first, the lowest id, the alphabetically smallest — would be a
    /// routing decision taken by a sort order.
    ///
    /// # Errors
    ///
    /// Returns an error when the catalog cannot be read, and
    /// [`Error::ManyWritablePeers`] when more than one peer is declared
    /// writable.
    ///
    /// # Why the whole row and not the endpoint
    ///
    /// Two callers want this peer and they want different fields of it: a
    /// forwarded write needs somewhere to send the statement, and a follower
    /// collecting the log needs the node id as well, because a peer connection
    /// derives the name it demands of the peer's certificate from that id. A
    /// second finder for the second field would be a second answer to *which
    /// peer may write*, and the two would disagree the day somebody declared
    /// two writable peers and only one of them checked.
    pub fn writable_peer(&self) -> Result<Option<ReplicaDefinition>> {
        let mut transaction = self.store.begin()?;
        let mut writable = Catalog::new(&mut transaction)
            .replicas()?
            .into_iter()
            .filter(|peer| peer.roles.has(Roles::WRITABLE));
        let Some(found) = writable.next() else {
            return Ok(None);
        };
        if let Some(second) = writable.next() {
            return Err(Error::ManyWritablePeers {
                named: found.name,
                also: second.name,
            });
        }
        Ok(Some(found))
    }

    /// The member row this store's catalog keeps for `node`, if any names it.
    ///
    /// What a redirect is resolved against (ADR-0101): an error names the node
    /// to go to, and the row says where a **client** reaches that node — the
    /// peer door it was declared at is not somewhere a client can speak. Read
    /// from the catalog this node applied, so it answers with the peer link down.
    ///
    /// # Errors
    ///
    /// Returns an error when the catalog cannot be read.
    pub fn member(&self, node: &[u8; NODE_ID_LEN]) -> Result<Option<ReplicaDefinition>> {
        let mut transaction = self.store.begin()?;
        Ok(Catalog::new(&mut transaction)
            .replicas()?
            .into_iter()
            .find(|peer| peer.node.as_ref() == Some(node)))
    }
}
