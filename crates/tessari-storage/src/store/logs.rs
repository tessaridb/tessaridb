//! The logs this store keeps, and where each one stands.

use tessari_encoding::{
    AppliedPositionKey, KeyKind, LogId, LogKey, LogRecord, StoreKey, StoreValue,
    VersionPositionKey, Writer,
};
use tessari_kv::{KeyRange, ScanDirection, ScanRequest};
use tessari_types::{Epoch, Sequence};

use crate::catalog::Reach;
use crate::error::Result;

use super::Store;

impl Store {
    /// Every log this store holds, in key order.
    ///
    /// Read from the applied-position keys rather than from a registry kept
    /// beside them: a log exists exactly when something has been written into
    /// it, and that is exactly when its position key exists. A second list would
    /// be a second fact about the same thing, and the failure of a list that
    /// drifts is a log nobody backs up or replicates.
    ///
    /// **Asked of the store and never reasoned from the type** (Q-632). A home
    /// may hold no log, one, or — once it admits two writers — several, and
    /// none of that is derivable from a [`Reach`].
    ///
    /// # Errors
    ///
    /// Returns the substrate's failure, and a decoding failure when a key in
    /// that keyspace is not an applied position.
    pub fn logs(&self) -> Result<Vec<LogId>> {
        let prefix = vec![KeyKind::AppliedPosition.tag()];
        self.logs_under(&prefix)
    }

    /// Every log of one home, in key order.
    ///
    /// # Errors
    ///
    /// The same as [`Self::logs`].
    pub fn logs_of(&self, home: Reach) -> Result<Vec<LogId>> {
        self.logs_under(&AppliedPositionKey::prefix_for_home(home))
    }

    /// The logs whose position keys carry `prefix`.
    pub(super) fn logs_under(&self, prefix: &[u8]) -> Result<Vec<LogId>> {
        let request = ScanRequest {
            keyspace: AppliedPositionKey::keyspace(),
            range: KeyRange::prefix(prefix),
            direction: ScanDirection::Forward,
            limit: None,
        };
        self.backend
            .scan(&request)?
            .into_iter()
            .map(|(key, _)| Ok(AppliedPositionKey::decode(key.as_slice())?.log))
            .collect()
    }

    /// The writer this node allocates positions as.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NoIdentity`] when the store holds no identity, which is
    /// a store that was never opened rather than a node without a name.
    pub fn writer(&self) -> Result<Writer> {
        Ok(Writer::new(self.node_identity()?.id))
    }

    /// This node's own log for `home`.
    ///
    /// The log a commit here allocates into, and the one a reporting caller
    /// means when it asks *how far is this range* without naming a writer.
    ///
    /// # Errors
    ///
    /// The same as [`Self::writer`].
    pub fn own_log(&self, home: Reach) -> Result<LogId> {
        Ok(LogId::new(home, self.writer()?))
    }

    /// The highest sequence committed in one home's log.
    ///
    /// While a commit and its application are the same event — which they are
    /// until the replication log separates them — the committed tail *is* the
    /// applied position, so no second key exists for it.
    ///
    /// **It counts in `home`'s log and nowhere else.** There is no store-wide
    /// answer to ask for: once each home allocates from its own counter, the
    /// largest number across homes is the larger of two unrelated counts, and
    /// the sum is a quantity no reader resumes from.
    ///
    /// # Errors
    ///
    /// Returns an error when the value cannot be read or decoded.
    pub fn committed_tail(&self, log: LogId) -> Result<Sequence> {
        let key = AppliedPositionKey::new(log).encode();
        let stored = self.backend.get(AppliedPositionKey::keyspace(), &key)?;
        match stored {
            Some(value) => Ok(Sequence::decode(value.as_slice())?),
            None => Ok(Sequence::ZERO),
        }
    }

    /// The furthest any log of `home` reaches other than `own`.
    ///
    /// `None` when `own` is the only log there, which is what a store standing
    /// alone looks like and is therefore the answer that must NOT be a zero:
    /// the two states this separates are *nothing has been written here* and
    /// *everything here was written by somebody else*, and they were one
    /// sentence until Q-764 measured what that sentence invites.
    ///
    /// A maximum rather than a sum, and not offered as a position anybody
    /// resumes from: counters in different logs are unrelated, so this says
    /// *there is history here and it reaches at least this far* and nothing
    /// more. The scan costs one prefix walk of the position keyspace, which is
    /// bounded by the number of writers in the home and not by the store.
    ///
    /// # Errors
    ///
    /// Returns the backend's failure when the logs or their tails cannot be
    /// read.
    pub(super) fn furthest_other_log(&self, home: Reach, own: LogId) -> Result<Option<Sequence>> {
        let mut furthest: Option<Sequence> = None;
        for log in self.logs_of(home)? {
            if log == own {
                continue;
            }
            let tail = self.committed_tail(log)?;
            furthest = Some(furthest.map_or(tail, |held| held.max(tail)));
        }
        Ok(furthest)
    }

    /// The newest version this store has written a record at.
    ///
    /// The twin of [`Self::committed_tail`], and the distinction between them
    /// is the whole of Q-614. The committed tail is the **log's** position: a
    /// replica resumes from it, a divergence is detected by comparing it, and
    /// it is therefore a number several nodes must agree on. This is **this
    /// store's** own: it orders this store's records against each other and
    /// against the snapshot a reader holds, and no other node ever reads it.
    ///
    /// They carry the same value while one leader decides every write, which is
    /// the only reason they were one key. Once positions are allocated per
    /// range, a snapshot taken from a log position would read one range as of
    /// its fifth record and another as of its fifth — two unrelated moments
    /// presented as one, with no error and plausible data.
    ///
    /// # Errors
    ///
    /// Returns an error when the value cannot be read or decoded.
    pub fn committed_version(&self) -> Result<Sequence> {
        let key = VersionPositionKey.encode();
        let stored = self.backend.get(VersionPositionKey::keyspace(), &key)?;
        match stored {
            Some(value) => Ok(Sequence::decode(value.as_slice())?),
            None => Ok(Sequence::ZERO),
        }
    }

    /// The leadership under which the newest record this node holds was written.
    ///
    /// Raft's `lastLogTerm`, and it is a different fact from [`Self::leading`].
    /// `leading` says *which epoch a majority granted THIS node*, which is
    /// `None` on a follower that has never campaigned however much history it
    /// holds. This says *which leadership wrote the last thing here*, which is
    /// what an election restriction has to compare: a follower carrying the
    /// newest records must not read as behind a node that once led and then
    /// fell away. Ordering a candidate by the wrong one of the two inverts the
    /// answer exactly where it matters.
    ///
    /// [`Epoch::ZERO`] for an empty log, which is the same value a store that
    /// has elected nobody holds — so the first record of a fresh log needs no
    /// special case at any call site, and it is the convention
    /// `refuse_a_parted_history` already uses one position further back.
    ///
    /// Costs one point read and a fixed eight-byte inspection: the epoch sits at
    /// a known offset in the stored record and the mutations are never decoded.
    ///
    /// # Errors
    ///
    /// Returns an error when the tail cannot be read, or when the record stored
    /// at it cannot be inspected — which is corruption rather than an absence.
    pub fn tail_leadership(&self, log: LogId) -> Result<Epoch> {
        let tail = self.committed_tail(log)?;
        if tail == Sequence::ZERO {
            return Ok(Epoch::ZERO);
        }
        let stored = self
            .backend
            .get(LogKey::keyspace(), &LogKey::new(log, tail).encode())?;
        // A tail naming a record the log does not hold is the retention case
        // Q-529 owns, and the honest answer here is the same one
        // `refuse_a_parted_history` gives: nothing to compare against. A node
        // that cannot state the leadership of its own tail is treated as
        // holding none, which makes it lose every comparison rather than win
        // one it cannot support.
        let Some(value) = stored else {
            return Ok(Epoch::ZERO);
        };
        Ok(LogRecord::epoch_in(value.as_slice())?)
    }
}
