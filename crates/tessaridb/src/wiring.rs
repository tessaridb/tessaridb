//! What the serving process plugs into a database: the commit signal, the cluster's
//! carriers and coordinator, the backup folder and the unseal period.

use super::*;

impl Db {
    /// Every landing in this store, announced: a commit through any session or
    /// surface, and a record applied from another writer's stream.
    ///
    /// One per database, so every feed on every surface of a process waits on
    /// the same announcement, and a change none of them caused still wakes
    /// them.
    #[must_use]
    pub fn commits(&self) -> &Arc<feed::Commits> {
        self.commits.get_or_init(|| {
            let commits = Arc::new(feed::Commits::default());
            let announced = Arc::clone(&commits);
            self.store.when_landed(move || announced.signal());
            commits
        })
    }

    /// Gather the shards of a split table this node lacks through `gather`
    /// (G033, ADR-0083), in every session opened from now on.
    ///
    /// Once per process: which peers exist is fixed when the node starts, and a
    /// second gatherer arriving later would mean two answers to one question.
    /// Answers `false`, and changes nothing, when one was already set.
    pub fn gather_through(&self, gather: Arc<dyn tessari_session::Gather>) -> bool {
        self.gather.set(gather).is_ok()
    }

    /// Let every session opened here carry the records of a transaction across
    /// leaders through `participants` (ADR-0112). Once per process, as
    /// [`Db::gather_through`]; answers `false`, and changes nothing, when it
    /// was already set.
    pub fn participating_through(
        &self,
        participants: Arc<dyn tessari_session::Participants>,
    ) -> bool {
        self.participants.set(participants).is_ok()
    }

    /// Let every session opened here know its peers through `elsewhere`
    /// (Q-863). Once per process, as [`Db::gather_through`]; answers `false`,
    /// and changes nothing, when it was already set.
    pub fn among(&self, elsewhere: Arc<dyn tessari_session::Elsewhere>) -> bool {
        self.elsewhere.set(elsewhere).is_ok()
    }

    /// Count every session's sign-in tries against the cluster's one budget
    /// (ADR-0108 D5). Once per process, as [`Db::gather_through`].
    pub fn budget_through(&self, budget: Arc<dyn tessari_session::Budget>) -> bool {
        self.budget.set(budget).is_ok()
    }

    /// Report the certificates `certificates` reads in every session's
    /// `INFO FOR NODE` (ADR-0108 D9). Once per process, as [`Db::gather_through`].
    pub fn presenting(&self, certificates: Arc<dyn tessari_session::Certificates>) -> bool {
        self.certificates.set(certificates).is_ok()
    }

    /// Carry a request this node cannot answer to the node that can, through
    /// `coordinate` (ADR-0108 D1). Once per process, as [`Db::gather_through`];
    /// answers `false`, and changes nothing, when one was already set.
    pub fn coordinate_through(&self, coordinate: Arc<dyn Coordinate>) -> bool {
        self.coordinate.set(coordinate).is_ok()
    }

    /// The coordinator, when this node has one.
    #[must_use]
    pub fn coordinator(&self) -> Option<&Arc<dyn Coordinate>> {
        self.coordinate.get()
    }

    /// The node that could answer what `refused` refused here, when it names
    /// one (ADR-0108 D1): the leader a write or a bounded read belongs to, a
    /// peer holding the whole of a split table, or the one peer declared
    /// writable when this node may not write.
    #[must_use]
    pub async fn answers_instead(
        &self,
        refused: &Error,
    ) -> Option<[u8; tessari_storage::NODE_ID_LEN]> {
        match refused {
            Error::ReadIsElsewhere { node, .. }
            | Error::Store(tessari_storage::Error::WriteIsElsewhere { node, .. }) => Some(*node),
            Error::NotHeldHere {
                holder: Some(holder),
                ..
            }
            | Error::ShardMapMoved {
                holder: Some(holder),
                ..
            } => Some(holder.node),
            // The store line's leader first: a cluster that can fail over
            // declares several writable rows, and "the one writable row" then
            // names nobody (DB-16). The row stays the answer before any
            // leadership has been recorded.
            Error::NotWritable { .. } => match self.store.leader_of(Reach::Store).await {
                Ok(Some(leader)) => Some(leader),
                _ => self.writable_peer().await.ok().flatten()?.node,
            },
            _ => None,
        }
    }

    /// Let `BACKUP … TO` write into `folder`, in every session opened from now on.
    ///
    /// Once per process, like [`Db::gather_through`]: where this machine keeps
    /// its backups is fixed when the node starts. Answers `false`, and changes
    /// nothing, when a folder was already set.
    pub fn back_up_into(&self, folder: Arc<Path>) -> bool {
        self.backups.set(folder).is_ok()
    }

    /// How long every later unseal of this store lasts (ADR-0092 D4).
    ///
    /// Ten minutes unless set. A key already held keeps the deadline it was
    /// given when it arrived.
    pub fn unseal_for(&self, period: core::time::Duration) {
        self.store.vault().last_for(period);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use tessari_storage::Now;
    use tessari_storage::{Catalog, Roles};

    const OTHER: [u8; tessari_storage::NODE_ID_LEN] = [7; tessari_storage::NODE_ID_LEN];

    /// A write refused because this node may not take one is sent to the node
    /// leading the store line — not to "the one writable row", which a cluster
    /// that can fail over never has (DB-16: two writable rows answered the
    /// refusal with `ManyWritablePeers` and named nobody to go to).
    #[test]
    fn a_refused_write_is_sent_to_the_store_lines_leader() {
        let db = Db::in_memory().now().unwrap();
        let mut transaction = db.store.begin().unwrap();
        let mut catalog = Catalog::new(&mut transaction);
        let both = Roles::SERVING.and(Roles::WRITABLE).and(Roles::COORDINATING);
        for (name, node) in [("n1", OTHER), ("n2", [8; tessari_storage::NODE_ID_LEN])] {
            catalog
                .create_replica(
                    name,
                    "127.0.0.1:1",
                    both,
                    Some(node),
                    Some(Reach::Store),
                    None,
                )
                .now()
                .unwrap();
        }
        catalog
            .record_leadership(Reach::Store, OTHER, tessari_types::Epoch::new(3))
            .unwrap();
        transaction.commit().now().unwrap();

        let refused = Error::NotWritable {
            span: tessari_ql::Span::new(0, 1),
        };
        assert_eq!(db.answers_instead(&refused).now(), Some(OTHER));
    }
}
