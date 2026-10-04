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
    pub fn answers_instead(&self, refused: &Error) -> Option<[u8; tessari_storage::NODE_ID_LEN]> {
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
            Error::NotWritable { .. } => self.writable_peer().ok().flatten()?.node,
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
