//! The store: the handle that owns the backend and the committed tail.
//!
//! One type owns the substrate handle for its lifetime, resolves the store's
//! on-disk format at open, and hands out transactions. Everything above it
//! speaks in records and sequences; nothing above it sees a key, a keyspace or
//! a batch.

use std::ops::Bound;
use std::sync::Arc;

use tessari_encoding::{
    AppliedPositionKey, FormatVersion, FormatVersionKey, LogKey, LogRecord, NodeIdentity, Roles,
    StoreKey, StoreValue,
};
use tessari_kv::{KeyRange, KvBackend, ScanDirection, ScanRequest, WriteBatch};
use tessari_types::Sequence;

use crate::error::{Error, Result};
use crate::feed::Changes;
use crate::snapshots::Registry;
use crate::transaction::Transaction;

/// What a store says about itself when asked.
///
/// Deliberately small. A store that reports everything it knows is a metrics
/// endpoint, which is a different thing with a different audience; this answers
/// the one question a load balancer and a pager both ask.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Health {
    /// Background failures the engine has recorded.
    ///
    /// Any at all is unwell. There is no threshold to tune, because a single
    /// background error means some flush or compaction did not happen, and a
    /// store that has stopped keeping its own promises is not less unwell for
    /// having stopped only once.
    pub background_errors: u64,
    /// The log position every committed write is at or below.
    ///
    /// Carried because "the process is up" and "the store is readable" are
    /// different claims and only the second one is useful.
    pub committed: Sequence,
}

impl Health {
    /// Whether anything is wrong.
    #[must_use]
    pub const fn is_well(&self) -> bool {
        self.background_errors == 0
    }

    /// What is wrong, for somebody reading it at three in the morning.
    #[must_use]
    pub fn complaint(&self) -> Option<String> {
        if self.is_well() {
            return None;
        }
        Some(format!(
            "{} background error(s): a flush or compaction has failed, so this store \
             is answering reads while it has stopped keeping them",
            self.background_errors
        ))
    }
}

/// A record store over a key-value backend.
///
/// Cloning a store shares one backend **and one snapshot registry**: two handles
/// to the same store are not two stores, and a floor computed from half the live
/// readers would reclaim versions the other half is still reading.
#[derive(Debug, Clone)]
pub struct Store {
    backend: Arc<dyn KvBackend>,
    snapshots: Arc<Registry>,
    /// What this process is doing with the declared consumers.
    ///
    /// Shared like the snapshot registry and for the same reason: a session
    /// answering `INFO FOR CONSUMER` and the thread doing the consuming must be
    /// looking at one registry, not at two that agree until they do not.
    running: Arc<crate::running::Running>,
    /// Whether this process can open what its vaults hold.
    ///
    /// Shared for the same reason as the two registries above, and the
    /// consequence of getting it wrong is larger: two handles to one store with
    /// two keyrings means one connection unseals and the next one is still
    /// sealed, which reads as an intermittent authorization fault rather than
    /// as the design error it is.
    ///
    /// It is **per-process and never persisted**. The root record travels in
    /// the log because every node needs it; the unsealed master key travels
    /// nowhere, so a follower holding every byte of the leader's log holds
    /// nothing that opens a secret.
    vault: Arc<crate::vault::OpenVault>,
    /// Where a read of a vault is recorded before its answer leaves.
    ///
    /// Beside the vault rather than inside it: the trail outlives any one
    /// unsealing, and a sealed store still records the reads it refused.
    audit: Arc<crate::audit::AuditTrail>,
}

impl Store {
    /// Open a store on `backend`, creating its metadata if it is new.
    ///
    /// A store whose on-disk format is newer than this build understands is
    /// **refused**. Opening it anyway would write this build's format into it,
    /// which is not recoverable afterwards.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails, when the stored metadata cannot
    /// be decoded, or when the on-disk format is newer than this build.
    pub fn open(backend: Arc<dyn KvBackend>) -> Result<Self> {
        // The format is settled before anything else is written, the node
        // identity included: a store this build is about to refuse must not be
        // modified on the way to refusing it.
        match read_format_version(&backend)? {
            Some(found) => found.check_supported()?,
            None => write_initial_metadata(&backend)?,
        }
        crate::node::ensure(&backend)?;
        Ok(Self {
            backend,
            snapshots: Arc::new(Registry::default()),
            running: Arc::new(crate::running::Running::default()),
            // Sealed. A store that opened unsealed would be one that opens
            // secrets for whoever restarted it.
            vault: Arc::new(crate::vault::OpenVault::sealed()),
            audit: Arc::new(crate::audit::AuditTrail::default()),
        })
    }

    /// Who this node is.
    ///
    /// Generated once into the `META` keyspace and stable across restarts, so an
    /// id that changed would be a session token rather than an identity. It is
    /// not in the log and therefore not in a backup — a restore onto a fresh
    /// store produces a different node, which is the whole point of the split
    /// (ADR-0018 §1).
    ///
    /// # Read every time, and deliberately not cached
    ///
    /// This used to be resolved once at open, on the reasoning that it never
    /// changes while the store is open. `DEFINE NODE` makes that false, and a
    /// cache that is *usually* right is worse here than no cache: the last one
    /// produced a restore test that compared two handles and passed while the
    /// bytes on disk were wrong, because the value being asserted on had been
    /// read before the restore ran. The identity is small, the read is rare —
    /// `$node` and `INFO FOR NODE` are administrative — and one source of truth
    /// costs less than a second one that must be kept in step.
    ///
    /// # Errors
    ///
    /// Returns the substrate's failure, [`Error::NoIdentity`] when the key has
    /// gone, or a decoding failure when the stored bytes carry a revision, role
    /// or membership this build does not know.
    pub fn node_identity(&self) -> Result<NodeIdentity> {
        crate::node::read(&self.backend)?.ok_or(Error::NoIdentity)
    }

    /// What this process is doing with the consumers the catalog declares.
    ///
    /// Empty until the runner starts something, and empty again after a restart
    /// — nothing here is persisted, because a persisted `running` flag outlives
    /// the thread it describes and the next process reads it as true.
    #[must_use]
    pub fn running(&self) -> &Arc<crate::running::Running> {
        &self.running
    }

    /// Where a read of a vault is recorded before its answer leaves.
    ///
    /// Handing this out is safe in a way handing out a key is not: what a caller
    /// can do with it is add a device that must also succeed for a read to be
    /// served. There is no way through it to make a read unrecorded.
    #[must_use]
    pub fn audit(&self) -> &Arc<crate::audit::AuditTrail> {
        &self.audit
    }

    /// Whether this process can open what the store's vaults hold.
    ///
    /// Sealed after every restart, deliberately: unsealing is the one thing
    /// nobody can automate away without also removing the property that makes a
    /// restart safe.
    #[must_use]
    pub fn vault(&self) -> &Arc<crate::vault::OpenVault> {
        &self.vault
    }

    /// Change what this node is for, and where it is reached.
    ///
    /// Absent arguments leave their field alone. Applied to the `META` keyspace
    /// immediately rather than through the transaction the statement runs in —
    /// the shape `BACKUP` already has, and for the same reason: `META` is not the
    /// log, so a write here cannot be part of a log transaction and pretending
    /// otherwise would be a durability claim the substrate does not support.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NoIdentity`] when the store holds none, and the
    /// substrate's failure when the write is refused.
    pub fn configure_node(
        &self,
        roles: Option<Roles>,
        endpoints: Option<Vec<String>>,
    ) -> Result<NodeIdentity> {
        crate::node::configure(&self.backend, roles, endpoints)
    }

    /// Begin a transaction at the current committed tail.
    ///
    /// # Errors
    ///
    /// Returns an error when the committed tail cannot be read or decoded.
    pub fn begin(&self) -> Result<Transaction<'_>> {
        Ok(Transaction::new(self, self.committed_tail()?))
    }

    /// Begin a transaction reading the store as it stood at `at`.
    ///
    /// Records are versioned by a suffix on their own key, so reading the past
    /// is the read this store already performs with a different sequence — not
    /// a second mechanism. What has to be added is the honesty about when it
    /// cannot be done.
    ///
    /// Two refusals, and they are refusals rather than best-effort answers
    /// because both alternatives are a plausible wrong number that nothing
    /// reports:
    ///
    /// - **Below the reclaim floor.** Reclamation removed the versions that
    ///   would have answered, so the read would resolve to something older, or
    ///   to nothing, and call that the past.
    /// - **Above the committed tail.** There is no state there yet. Answering
    ///   with the present would make a read of the future silently succeed and
    ///   then change its answer the next time it is asked.
    ///
    /// # Errors
    ///
    /// Returns [`Error::VersionReclaimed`] when `at` is below the reclaim floor,
    /// [`Error::VersionInTheFuture`] when it is above the committed tail, or a
    /// backend error when either bound cannot be read.
    pub fn begin_at(&self, at: Sequence) -> Result<Transaction<'_>> {
        let floor = self.reclaim_floor()?;
        if at < floor {
            return Err(Error::VersionReclaimed {
                asked: at.get(),
                floor: floor.get(),
            });
        }
        let tail = self.committed_tail()?;
        if at > tail {
            return Err(Error::VersionInTheFuture {
                asked: at.get(),
                tail: tail.get(),
            });
        }
        Ok(Transaction::new(self, at))
    }

    /// The oldest sequence any live reader can still need.
    ///
    /// Versions strictly older than the newest version at or below this may be
    /// reclaimed; nothing at or above it may be. With no reader live the floor is
    /// the committed tail, because a transaction that begins next will begin
    /// there.
    ///
    /// # Errors
    ///
    /// Returns an error when the committed tail cannot be read, which is only
    /// consulted when no snapshot is live.
    pub fn retention_floor(&self) -> Result<Sequence> {
        match self.snapshots.oldest() {
            Some(oldest) => Ok(oldest),
            None => self.committed_tail(),
        }
    }

    /// How long the oldest live snapshot has been held, if one is.
    ///
    /// ADR-0005 §9 calls snapshot lifetime an operational limit rather than an
    /// application detail, because a long-held snapshot postpones every tombstone
    /// in the store. This is the value that limit is checked against.
    #[must_use]
    pub fn oldest_snapshot_age(&self) -> Option<std::time::Duration> {
        self.snapshots.oldest_age()
    }

    /// How many distinct snapshots are being read from.
    #[must_use]
    pub fn live_snapshots(&self) -> usize {
        self.snapshots.len()
    }

    /// The registry a transaction registers itself with.
    pub(crate) fn snapshot_registry(&self) -> &Arc<Registry> {
        &self.snapshots
    }

    /// Whether this store is well, and what is wrong when it is not.
    ///
    /// # Why this exists rather than a metric
    ///
    /// An engine does its compaction, its flushing and its write-ahead work on
    /// its own threads, and a failure there surfaces at **no call a caller
    /// makes**. The store keeps answering reads while the thing that keeps it
    /// durable has stopped. That is the one failure this store cannot detect by
    /// being used, so something has to ask.
    ///
    /// # Where the alert lives, and why it is not here
    ///
    /// Not here. This answers *what is true*; deciding it is worth waking
    /// somebody for belongs to whatever already wakes people. The HTTP surface
    /// turns an unwell store into a failing `GET /health`, which every load
    /// balancer takes out of rotation and every monitor pages on — so the alert
    /// is the one that already exists rather than a second one written here and
    /// tested never.
    ///
    /// # Errors
    ///
    /// Returns the backend's failure when the counts cannot be read.
    pub fn health(&self) -> Result<Health> {
        Ok(Health {
            background_errors: self.backend.background_errors()?,
            committed: self.committed_tail()?,
        })
    }

    /// The highest sequence that has been committed.
    ///
    /// While a commit and its application are the same event — which they are
    /// until the replication log separates them — the committed tail *is* the
    /// applied position, so no second key exists for it.
    ///
    /// # Errors
    ///
    /// Returns an error when the value cannot be read or decoded.
    pub fn committed_tail(&self) -> Result<Sequence> {
        let key = AppliedPositionKey.encode();
        let stored = self.backend.get(AppliedPositionKey::keyspace(), &key)?;
        match stored {
            Some(value) => Ok(Sequence::decode(value.as_slice())?),
            None => Ok(Sequence::ZERO),
        }
    }

    /// Read log records from `from` onward, oldest first.
    ///
    /// `limit` bounds the read because a log is unbounded by nature and a caller
    /// that asks for "the rest of it" is asking for however much has accumulated
    /// since it last looked.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails or a stored record cannot be
    /// decoded.
    /// The record changes from `from` onward, oldest first.
    ///
    /// A projection of [`Store::log_records`] and nothing more: the feed holds no
    /// state, cannot disagree with what was committed, and is identical on a
    /// replica reading the same log. Catalog changes are not in it — a
    /// subscriber watching `users` did not ask for the rows that describe
    /// `users` — and a change says what a record *became* rather than whether it
    /// is new; both are explained in [`crate::feed`].
    ///
    /// `limit` bounds the **log records** read, not the changes produced, so one
    /// commit is never returned half-way: a subscriber applies a commit as the
    /// unit it was written as. For the same reason the answer carries the
    /// position to resume from — a commit that only touched the catalog yields
    /// no changes, and a reader given only a list could not tell that from
    /// "nothing has happened" and would ask for the same records forever.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails, or when a record or a payload
    /// cannot be decoded. A payload that cannot be decoded is corruption rather
    /// than a change to skip.
    pub fn changes_since(&self, from: Sequence, limit: usize) -> Result<Changes> {
        let records = self.log_records(from, limit)?;
        let next = records.last().map_or(from, |(sequence, _)| {
            Sequence::new(sequence.get().saturating_add(1))
        });
        let mut changes = Vec::new();
        for (sequence, record) in records {
            changes.extend(crate::feed::changes_in(sequence, &record)?);
        }
        Ok(Changes { changes, next })
    }

    /// Read log records from `from` onward, oldest first.
    ///
    /// `limit` bounds the read because a log is unbounded by nature and a caller
    /// that asks for "the rest of it" is asking for however much has accumulated
    /// since it last looked.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails or a stored record cannot be
    /// decoded.
    pub fn log_records(&self, from: Sequence, limit: usize) -> Result<Vec<(Sequence, LogRecord)>> {
        let prefix = LogKey::prefix();
        let bounds = KeyRange::prefix(&prefix);
        let request = ScanRequest {
            keyspace: LogKey::keyspace(),
            range: KeyRange::from_bounds(
                Bound::Included(LogKey::new(from).encode()),
                bounds.end().clone(),
            ),
            direction: ScanDirection::Forward,
            limit: Some(limit),
        };
        self.backend
            .scan(&request)?
            .into_iter()
            .map(|(key, value)| {
                let sequence = LogKey::decode(key.as_slice())?.sequence;
                let record = LogRecord::decode(value.as_slice())?;
                Ok((sequence, record))
            })
            .collect()
    }

    /// Apply one log record, at the sequence it carries.
    ///
    /// This is what a replica runs, and it is the same function a commit runs
    /// once it has decided its sequence locally.
    ///
    /// Re-applying a record the store already holds is a **no-op**, not an
    /// error: a replica that is re-sent a record it already has has not been
    /// told anything wrong, and refusing would turn an ordinary retry into an
    /// incident. Skipping *forward* is refused, because a gap means the state
    /// would no longer be explained by any log.
    ///
    /// # Errors
    ///
    /// Returns [`Error::LogGap`] when the record is not the next one, and the
    /// mapped backend or decoding failure otherwise.
    pub fn apply_record(&self, at: Sequence, record: &LogRecord) -> Result<()> {
        let applied = self.committed_tail()?;
        if at.get() <= applied.get() {
            return Ok(());
        }
        let expected = Sequence::new(applied.get().saturating_add(1));
        if at != expected {
            return Err(Error::LogGap {
                expected,
                found: at,
            });
        }
        // A replica re-checks what the leader already checked. That is cheap
        // relative to the apply, and a violation reaching this point is a
        // divergence between two nodes' catalogs rather than a caller's mistake
        // — which is worth stopping at rather than writing through.
        crate::schema::validate(self, record)?;
        let batch = crate::index::maintain(self, record, crate::log::apply_batch(at, record))?;
        // Derived here as well as in the commit, because that is the whole
        // reason it is derived from the record: a follower that skipped this
        // would carry the records and none of the counts, and its planner would
        // then choose a different access path for the same query.
        let batch = crate::cardinality::maintain(self, record, batch, at)?;
        self.backend.apply(batch)?;
        Ok(())
    }

    /// The backend, for the transaction's read and commit paths.
    pub(crate) fn backend(&self) -> &Arc<dyn KvBackend> {
        &self.backend
    }
}

/// The format this store was written in, if it has been written at all.
///
/// A free function rather than a method because it runs before the store
/// exists: `open` settles the format before it resolves the node identity, and
/// the identity is one of the store's own fields.
fn read_format_version(backend: &Arc<dyn KvBackend>) -> Result<Option<FormatVersion>> {
    let key = FormatVersionKey.encode();
    let stored = backend.get(FormatVersionKey::keyspace(), &key)?;
    match stored {
        Some(value) => Ok(Some(FormatVersion::decode(value.as_slice())?)),
        None => Ok(None),
    }
}

/// Write the metadata a fresh store needs, refusing if someone raced us.
///
/// The `Absent` precondition is what makes two processes opening the same
/// new store safe: exactly one of them writes the metadata.
fn write_initial_metadata(backend: &Arc<dyn KvBackend>) -> Result<()> {
    let format_key = FormatVersionKey.encode();
    let applied_key = AppliedPositionKey.encode();
    let batch = WriteBatch::new()
        .expect_absent(FormatVersionKey::keyspace(), format_key.clone())
        .put(
            FormatVersionKey::keyspace(),
            format_key,
            FormatVersion::CURRENT.encode(),
        )
        .put(
            AppliedPositionKey::keyspace(),
            applied_key,
            Sequence::ZERO.encode(),
        );
    backend.apply(batch)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    // Test assertions are exactly where a panic is the correct outcome.
    #![allow(clippy::panic, clippy::unwrap_used)]

    use tessari_kv::MemoryBackend;

    use super::*;
    use crate::error::Error;

    fn backend() -> Arc<dyn KvBackend> {
        Arc::new(MemoryBackend::new())
    }

    #[test]
    fn a_fresh_store_writes_its_format_and_starts_at_sequence_zero() {
        let store = Store::open(backend()).unwrap();
        assert_eq!(store.committed_tail().unwrap(), Sequence::ZERO);
        assert_eq!(
            read_format_version(store.backend()).unwrap(),
            Some(FormatVersion::CURRENT)
        );
    }

    #[test]
    fn reopening_a_store_does_not_rewrite_its_metadata() {
        let shared = backend();
        let first = Store::open(Arc::clone(&shared)).unwrap();
        drop(first);
        let second = Store::open(shared).unwrap();
        assert_eq!(
            read_format_version(second.backend()).unwrap(),
            Some(FormatVersion::CURRENT)
        );
    }

    #[test]
    fn a_newer_on_disk_format_is_refused_rather_than_opened() {
        let shared = backend();
        let future = FormatVersion::new(FormatVersion::CURRENT.get().saturating_add(1));
        shared
            .apply(WriteBatch::new().put(
                FormatVersionKey::keyspace(),
                FormatVersionKey.encode(),
                future.encode(),
            ))
            .unwrap();

        let error = Store::open(shared).unwrap_err();
        assert_eq!(error.code(), "incompatible");
        assert!(!error.is_retryable());
        match error {
            Error::Encoding(inner) => {
                assert!(inner.to_string().contains("format version"), "{inner}");
            }
            other => panic!("unexpected error: {other}"),
        }
    }
}
