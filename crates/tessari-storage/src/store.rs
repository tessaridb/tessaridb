//! The store: the handle that owns the backend and the committed tail.
//!
//! One type owns the substrate handle for its lifetime, resolves the store's
//! on-disk format at open, and hands out transactions. Everything above it
//! speaks in records and sequences; nothing above it sees a key, a keyspace or
//! a batch.

use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use tessari_encoding::{
    AppliedPositionKey, FormatVersion, FormatVersionKey, KeyKind, LogId, LogKey, NODE_ID_LEN,
    REACH_LEN, StoreKey, StoreValue, VersionPositionKey, Writer,
};
use tessari_kv::{Key, KeyRange, Keyspace, KvBackend, ScanDirection, ScanRequest, WriteBatch};
use tessari_types::{Epoch, Sequence};

use crate::catalog::Reach;

/// The home read by the surfaces that still answer with **one** number for the
/// whole node — health, follower lag, the tail mark.
///
/// Parked rather than decided (Q-622). Once every home counts from its own
/// counter, "how far along is this node" has no single answer: the largest
/// position across homes compares unrelated counters, and the sum is a quantity
/// nothing resumes from. These surfaces feed the greeting and the operator
/// report, both of which are the wire's to change, so B2 leaves them reading the
/// store's own log and names the fact here instead of spreading the same comment
/// across three call sites.
pub(crate) const UNPARTITIONED_REPORT_HOME: Reach = Reach::Store;
use crate::error::Result;
use crate::followers::Followers;
use crate::snapshots::Registry;

mod adoption;
mod apply;
mod history;
mod leadership;
pub(crate) use leadership::Led;
mod leases;
mod lines;
mod logs;
mod opening;
mod parts;
mod reporting;
mod reseeding;
mod restoring;

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
    /// The log position every write THIS NODE committed is at or below.
    ///
    /// Carried because "the process is up" and "the store is readable" are
    /// different claims and only the second one is useful.
    ///
    /// **This node's own log, and once a home admits more than one writer that
    /// is not the same as the store's history.** It used to be documented as
    /// the position every committed write is at or below, which stopped being
    /// true when a log took its writer's name: a follower's records sit in its
    /// leader's log, and a store migrated from before writers were named holds
    /// every record it ever had in a log attributed to nobody. Both are
    /// unreadable from this number alone, which is what [`Health::elsewhere`]
    /// is for.
    pub committed: Sequence,
    /// The furthest any OTHER log of the same home reaches, when there is one.
    ///
    /// `None` says this node's log is the only one here, which is the ordinary
    /// state of a store standing alone — and it is the reason this is an option
    /// rather than a zero, because a store nobody has written and a store whose
    /// history belongs to somebody else must not answer the same thing (Q-764).
    pub elsewhere: Option<Sequence>,
    /// Log divergences this process has refused.
    ///
    /// Not persisted, for the reason `crate::running`'s header gives about
    /// anything else that describes a process rather than a store: a count that
    /// outlived the process that observed it would be a claim nobody can check.
    /// It is here rather than nowhere because a detector added after the first
    /// incident is a detector that was absent during it.
    pub log_divergences: u64,
    /// Writes discarded by a table that declared `LAST WRITER WINS`.
    ///
    /// The only record that a write stopped being reachable. W292 measured what
    /// its absence looks like: the losing version stays on disk byte-intact and
    /// disappears from every answer, so an operator auditing storage for data
    /// loss finds both versions and concludes nothing was lost. Counting it at
    /// the commit is what makes a declared last-writer-wins honest rather than
    /// silent (ADR-0075, G027 S3.2).
    ///
    /// Not persisted, for the reason [`Health::log_divergences`] gives.
    pub discarded_writes: u64,
    /// Leadership rounds this node has stood in.
    ///
    /// Here for the reason the divergence count is, and for one more: a healthy
    /// cluster is supposed to be QUIET. A follower that can hear a leader does
    /// not stand against it (ADR-0066), so this number staying flat while a
    /// leader holds its lease is the observable form of that rule — and a
    /// cluster that has started campaigning against a live leader is spending
    /// the one resource an election needs, which is the willingness of its
    /// voters to grant anything.
    ///
    /// It counts rounds STOOD, not rounds won. A round that loses is exactly
    /// the noise this is here to make visible.
    pub campaigns: u64,
    /// How long this node may still write under its lease, or `None` when it
    /// holds none.
    ///
    /// Here beside the divergence count and for the same reason: this is where
    /// the detectors live, and the concept names *lease remaining* as the
    /// split-brain signal because the dangerous state is exactly this reaching
    /// zero while writes are still being accepted. `None` says no lease was
    /// granted, which is the ordinary state of a store standing alone and is
    /// not a value of zero.
    pub lease_remaining: Option<std::time::Duration>,
    /// Reads that reached this node holding none of what they asked for
    /// (`NotHeldHere`), since this process opened the store (G053 C6).
    ///
    /// A client routing well keeps this near zero; a number climbing says the
    /// reads are aimed at the wrong node, which nothing else here shows.
    pub not_held_here: u64,
    /// Commits that waited for a majority of voters to hold them (G053 C6).
    pub acknowledgement_waits: u64,
    /// Of those, the ones answered `NotAcknowledgedInTime` — committed here,
    /// not confirmed by a majority within one round.
    pub acknowledgement_timeouts: u64,
    /// The time those waits took, summed, so a scrape divides it by the count.
    pub acknowledgement_waited: std::time::Duration,
    /// Transactions across leaders this node coordinated that committed, since
    /// this process opened the store (ADR-0112 D11).
    pub across_committed: u64,
    /// Of those it coordinated, the ones that aborted.
    pub across_aborted: u64,
    /// And the ones whose decision was sent and not confirmed — the client was
    /// told the outcome is in doubt, and the record's range finishes it.
    pub across_in_doubt: u64,
    /// Transaction records `PENDING` here as the last settling pass left them,
    /// or `None` before the first pass — sampled on the pass's cadence rather
    /// than counted per request, because the records are kept for a while and a
    /// walk of them per scrape would grow with them.
    pub across_pending: Option<u64>,
    /// Transactions holding intents here as the last settling pass left them,
    /// or `None` before the first pass. Above zero for longer than a pass or
    /// two is a transaction waiting on a coordinator range that does not answer.
    pub across_with_intents: Option<u64>,
    /// Placements the leadership balancer moved since this process opened the
    /// store (ADR-0113 D3, D4).
    pub balancer_moves: u64,
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
    /// answering `INFO FOR KAFKA CONSUMER` and the thread doing the consuming must be
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
    /// How many times each name has lately missed signing in here.
    ///
    /// Shared with every handle for the vault keyring's reason — a reconnect
    /// must not get a fresh allowance — and held per store rather than per
    /// process because a name is an account in this store's catalog, and
    /// another store's misses are not guesses at it ([`crate::attempts`]).
    attempts: Arc<crate::attempts::Attempts>,
    /// Where a read of a vault is recorded before its answer leaves.
    ///
    /// Beside the vault rather than inside it: the trail outlives any one
    /// unsealing, and a sealed store still records the reads it refused.
    audit: Arc<crate::audit::AuditTrail>,
    /// Which tables carry a retention floor.
    ///
    /// Shared for the reason the registries above it are, and held in memory
    /// for the reason its own module states: the floor is asked on the hottest
    /// path there is, and a catalog read there would charge every table for a
    /// feature only a series has.
    series: Arc<crate::series::SeriesRegistry>,
    shards: Arc<crate::shards::ShardRegistry>,
    /// Table definitions already decoded, keyed by their stored bytes.
    decoded_tables: Arc<crate::catalog::DecodedTables>,
    /// Name and table rows valid for readers at or above the last change to them.
    catalog_rows: Arc<crate::catalog::CatalogRows>,
    served: Arc<crate::served::Served>,
    /// Whether any record version in this store has ever carried an expiry
    /// (G035). See [`crate::lapse`] for why the commit path asks.
    expiring: Arc<crate::lapse::Expiring>,
    /// How much of each `PUBLIC` topic's anonymous allowance is spent, on this
    /// node (G037). See [`crate::topic`]'s rate module for why it is not stored.
    public_appends: Arc<crate::topic::PublicRates>,
    /// Log divergences refused since this process opened the store.
    ///
    /// Shared with every handle for the same reason the snapshot registry is:
    /// two handles to one store are not two stores, and a count split between
    /// them is a count nobody can read.
    divergences: Arc<AtomicU64>,
    /// Writes discarded under a declared last-writer-wins since this process
    /// opened the store.
    ///
    /// Shared with every handle for the reason the divergence count is.
    discarded: Arc<AtomicU64>,
    /// Leadership rounds stood since this process opened the store.
    campaigns: Arc<AtomicU64>,
    /// `NotHeldHere` answers and majority waits since this process opened the
    /// store, shared with every handle for the reason the counters above are.
    tally: Arc<crate::tally::ClusterTally>,
    /// What the balancing pass last measured of each balanced table's shards
    /// (ADR-0113 D4), shared with every handle like the tally beside it.
    sampled: Arc<crate::sampled_shards::SampledShards>,
    /// How many log records this process keeps where no statement said
    /// (ADR-0094 D2). Shared with every handle for the reason the counters
    /// beside it are.
    retention: Arc<crate::retention::ProcessRetention>,
    /// Log positions held against pruning while a follower is copied.
    log_holds: Arc<crate::log_holds::LogHolds>,
    /// What this process has given each follower, and when.
    ///
    /// Shared with every handle for the reason the registries above it are, and
    /// held in memory for the reason `crate::followers` gives in its own
    /// header: a follower's progress is a fact about a live relationship, and a
    /// persisted copy of it would outlive the relationship it describes.
    followers: Arc<Followers>,
    /// What each follower has made durable of this leader's logs, which a write
    /// waiting for a majority reads (ADR-0106 D6). In memory for the same reason
    /// as `followers`.
    holds: Arc<crate::holds::Holds>,
    /// What this node has collected for itself, and when it was last level.
    ///
    /// Held in memory for the reason `crate::collections` gives in its own
    /// header, which is `crate::followers`' argument turned round: a restored
    /// process would publish a currency claim about a relationship it has not
    /// had for a week.
    collections: Arc<crate::collections::Collections>,
    /// The lease this process is writing under, if it was given one.
    ///
    /// Shared with every handle for the reason the registries above it are, and
    /// held in memory for a sharper one: a persisted fence is an unreplicated
    /// file asserting a cluster-wide fact, which is the split-brain ADR-0018 §1
    /// keeps out of `META`.
    lease: Arc<crate::lease::Held>,
    /// The epoch a majority granted this node, if one ever did.
    ///
    /// Beside the lease rather than inside it, because they answer different
    /// questions and `crate::lease` is deliberately about the fence alone: the
    /// lease says *until when this node may write*, the epoch says *which
    /// leadership it is writing under*. A greeting carries the second and a
    /// commit is refused by the first.
    ///
    /// `None` for a node that never won a round, which is every single-node
    /// store and every node before its first campaign — a different statement
    /// from epoch zero, exactly as `None` from the lease is a different
    /// statement from a spent one.
    leading: Arc<std::sync::Mutex<Option<Epoch>>>,
    /// The placed ranges this node leads on lines of their own (ADR-0082).
    ///
    /// Beside the store line's lease and epoch rather than replacing them, so a
    /// store with no placement never touches it and behaves as it always did.
    lines: Arc<crate::lines::Lines>,
    /// When this leader's own log reached each position.
    ///
    /// Shared with every handle for the reason the registries above it are, and
    /// held in memory for the sharpest of their reasons: it is a timeline of
    /// `Instant`s, which have no meaning outside the process that took them.
    /// See `crate::tailmarks` for why a follower's copy has no age without it.
    tailmarks: Arc<crate::tailmarks::TailMarks>,
    /// The turn every writer of a log record in this process takes, shared by
    /// every handle — see `crate::gate` for why writers queue rather than race.
    writing: Arc<crate::gate::WriteGate>,
    /// Who answers a reader meeting an intent its copy cannot decide
    /// (ADR-0112 D13d), installed once by the node — see `crate::decisions`.
    pub(crate) decisions: crate::decisions::Installed,
}

/// The format this store was written in, if it has been written at all.
///
/// A free function rather than a method because it runs before the store
/// exists: `open` settles the format before it resolves the node identity, and
/// the identity is one of the store's own fields.
fn read_format_version(backend: Arc<dyn KvBackend>) -> Result<Option<FormatVersion>> {
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
/// Refuse a store that holds data and no format version.
///
/// Every store this engine creates is stamped before anything else is written
/// to it, so data without a stamp is data whose format nobody knows; stamping
/// it as new would write this build's format over it. One key per keyspace is
/// read, which is what an empty store costs to tell apart from one that is not.
fn refuse_data_without_a_stamp(backend: &dyn KvBackend) -> Result<()> {
    for &keyspace in Keyspace::ALL {
        let first = backend.scan(&ScanRequest::new(keyspace, KeyRange::all()).with_limit(1))?;
        if !first.is_empty() {
            return Err(tessari_encoding::Error::UnstampedStore {
                keyspace: keyspace.name(),
            }
            .into());
        }
    }
    Ok(())
}

fn write_initial_metadata(backend: Arc<dyn KvBackend>) -> Result<()> {
    let format_key = FormatVersionKey.encode();
    let batch = WriteBatch::new()
        .expect_absent(FormatVersionKey::keyspace(), format_key.clone())
        .put(
            FormatVersionKey::keyspace(),
            format_key,
            FormatVersion::CURRENT.encode(),
        );
    backend.apply(batch)?;
    Ok(())
}

/// Rewrite a log written before it had homes, once, at open.
///
/// Every record in such a store was written by one leader into one flat log, so
/// [`Reach::Store`] is not a fallback for them — it is the home they actually
/// belong to, and the chain a narrower subscriber reads passes through it. The
/// keys are rewritten rather than read through a second decoder because the old
/// shape and the new one are told apart by length, and a choice made by length
/// on the replication read path is a choice made on every record forever.
///
/// One batch, so a store is either rewritten or untouched. The format version
/// moves inside it, which is what makes a failure retry on the next open
/// instead of leaving half a log in each shape.
///
/// # Errors
///
/// Returns the substrate's failure, and a decoding failure when a log key of
/// the expected old shape does not hold a sequence.
fn give_an_older_log_its_home(backend: Arc<dyn KvBackend>, found: FormatVersion) -> Result<()> {
    if found >= FormatVersion::HOMED_LOG {
        return Ok(());
    }
    let prefix = LogKey::prefix();
    let request = ScanRequest {
        keyspace: LogKey::keyspace(),
        range: KeyRange::prefix(&prefix),
        direction: ScanDirection::Forward,
        limit: None,
    };
    let mut batch = WriteBatch::new();
    for (key, value) in backend.scan(&request)? {
        let Some(sequence) = sequence_in_a_homeless_log_key(key.as_slice()) else {
            continue;
        };
        batch = batch.delete(LogKey::keyspace(), key).put(
            LogKey::keyspace(),
            LogKey::new(LogId::unattributed(Reach::Store), sequence).encode(),
            value,
        );
    }
    // The applied position moved the same way, from one singleton to one key
    // per home. Read before the rewrite for the same reason the records are:
    // its old key no longer names anything this build addresses.
    let homeless_applied = Key::from(vec![KeyKind::AppliedPosition.tag()]);
    if let Some(value) = backend.get(AppliedPositionKey::keyspace(), &homeless_applied)? {
        batch = batch
            .delete(AppliedPositionKey::keyspace(), homeless_applied)
            .put(
                AppliedPositionKey::keyspace(),
                AppliedPositionKey::new(LogId::unattributed(Reach::Store)).encode(),
                value,
            );
    }
    batch = batch.put(
        FormatVersionKey::keyspace(),
        FormatVersionKey.encode(),
        FormatVersion::HOMED_LOG.encode(),
    );
    backend.apply(batch)?;
    Ok(())
}

/// Rewrite a log written before its entries named their writer, once, at open.
///
/// # Why this rewrites rather than reads two shapes
///
/// The writer could have been written only where a range has two of them,
/// leaving a short key and a long one under one tag and telling them apart by
/// length on the read path. [`give_an_older_log_its_home`] already met that
/// choice for the home and refused it, in the sentence directly above: a choice
/// made by length there is a choice made on **every record forever**, instead of
/// once. The same refusal applies to the writer, and it buys more here — a
/// fixed-width writer is what keeps [`LogKey::prefix_for`] exact, and an exact
/// per-log prefix is what stops a scan of one writer's log quietly returning
/// another's.
///
/// # What writer an existing entry gets
///
/// [`Writer::UNATTRIBUTED`], and not this node's own identifier. A store being
/// migrated may be a follower, and a follower's log holds the records the
/// *leader* wrote — attributing them to the machine that happens to be opening
/// the file would be a lie the store then carries as fact. Nobody is the only
/// true answer available.
///
/// One batch, so a store is either rewritten or untouched, with the format
/// version moving inside it — which is what makes a failure retry on the next
/// open rather than leave half a log in each shape.
///
/// # Errors
///
/// Returns the substrate's failure, and a decoding failure when a log key of
/// the expected old shape does not hold a sequence.
fn give_an_older_log_its_writer(backend: Arc<dyn KvBackend>, found: FormatVersion) -> Result<()> {
    if found >= FormatVersion::WRITER_QUALIFIED_LOG {
        return Ok(());
    }
    let mut batch = WriteBatch::new();
    let entries = ScanRequest {
        keyspace: LogKey::keyspace(),
        range: KeyRange::prefix(&LogKey::prefix()),
        direction: ScanDirection::Forward,
        limit: None,
    };
    for (key, value) in backend.scan(&entries)? {
        let Some((home, sequence)) = an_unqualified_log_key(key.as_slice())? else {
            continue;
        };
        batch = batch.delete(LogKey::keyspace(), key).put(
            LogKey::keyspace(),
            LogKey::new(LogId::unattributed(home), sequence).encode(),
            value,
        );
    }
    // The applied position moved the same way, from one key per home to one per
    // log. Rewritten in the same batch as the entries it accounts for, because a
    // store holding one of the two shapes is a store whose next commit builds on
    // a tail nothing wrote.
    let positions = ScanRequest {
        keyspace: AppliedPositionKey::keyspace(),
        range: KeyRange::prefix(&[KeyKind::AppliedPosition.tag()]),
        direction: ScanDirection::Forward,
        limit: None,
    };
    for (key, value) in backend.scan(&positions)? {
        let Some(home) = an_unqualified_position_key(key.as_slice())? else {
            continue;
        };
        batch = batch.delete(AppliedPositionKey::keyspace(), key).put(
            AppliedPositionKey::keyspace(),
            AppliedPositionKey::new(LogId::unattributed(home)).encode(),
            value,
        );
    }
    batch = batch.put(
        FormatVersionKey::keyspace(),
        FormatVersionKey.encode(),
        FormatVersion::CURRENT.encode(),
    );
    backend.apply(batch)?;
    Ok(())
}

/// The home and sequence in a log key written before log keys carried a writer,
/// or `None` when the key is already qualified.
///
/// Told apart by length, which is exact **here** and nowhere else: both shapes
/// are fixed-width and differ by the sixteen bytes of the writer. That this is
/// a one-time read at open is the whole difference from making the same test on
/// the replication read path.
///
/// # Errors
///
/// Returns a decoding failure when a key of the old shape does not hold a
/// readable home.
fn an_unqualified_log_key(key: &[u8]) -> Result<Option<(Reach, Sequence)>> {
    const UNQUALIFIED_LEN: usize = 1 + REACH_LEN + 8;
    if key.len() != UNQUALIFIED_LEN {
        return Ok(None);
    }
    let home = reach_in(key)?;
    let Some(tail) = key.get(1usize.saturating_add(REACH_LEN)..) else {
        return Ok(None);
    };
    let Ok(bytes) = <[u8; 8]>::try_from(tail) else {
        return Ok(None);
    };
    Ok(Some((home, Sequence::new(u64::from_be_bytes(bytes)))))
}

/// The home in an applied-position key written before positions named a writer,
/// or `None` when the key is already qualified.
///
/// # Errors
///
/// Returns a decoding failure when a key of the old shape does not hold a
/// readable home.
fn an_unqualified_position_key(key: &[u8]) -> Result<Option<Reach>> {
    const UNQUALIFIED_LEN: usize = 1 + REACH_LEN;
    if key.len() != UNQUALIFIED_LEN {
        return Ok(None);
    }
    Ok(Some(reach_in(key)?))
}

/// The home an old-shape key leads with, read through the key type that owns
/// the encoding rather than by slicing bytes here.
///
/// The reach sits at the same offset in both keyspaces, so the bytes are
/// re-tagged as an applied position and decoded by the type that owns the
/// layout. Reading nine bytes here by hand would be a second decoder for one
/// encoding, which is the thing this layer exists to prevent.
fn reach_in(key: &[u8]) -> Result<Reach> {
    let mut qualified =
        Vec::with_capacity(1usize.saturating_add(REACH_LEN).saturating_add(NODE_ID_LEN));
    qualified.push(KeyKind::AppliedPosition.tag());
    qualified.extend_from_slice(
        key.get(1..1usize.saturating_add(REACH_LEN))
            .unwrap_or_default(),
    );
    qualified.extend_from_slice(&Writer::UNATTRIBUTED.bytes());
    Ok(AppliedPositionKey::decode(&qualified)?.log.home)
}

/// The sequence in a log key written before log keys carried a home, or `None`
/// when the key is already homed.
///
/// Told apart by length, which is exact: both shapes are fixed-width, and they
/// differ by the nine bytes of the home.
fn sequence_in_a_homeless_log_key(key: &[u8]) -> Option<Sequence> {
    const HOMELESS_LEN: usize = 9;
    if key.len() != HOMELESS_LEN {
        return None;
    }
    let bytes: [u8; 8] = key.get(1..HOMELESS_LEN)?.try_into().ok()?;
    Some(Sequence::new(u64::from_be_bytes(bytes)))
}

/// Give the version counter a value, once, on a store that has none.
///
/// Every store written before the version was separated from the log position
/// stamped its records at the position, so the position **is** the version
/// those records were written at. Seeding from it is what makes the counter
/// resume rather than restart — a counter that began again at zero would hand
/// out version numbers the store's existing records already hold, and a reader
/// would resolve to whichever of the two the key order happened to put first.
///
/// A fresh store reaches this with the position at zero, so the two cases are
/// one path rather than two that could disagree.
///
/// The `Absent` precondition makes a race between two openers harmless: one
/// writes, the other is refused and finds the value already there. A refusal is
/// therefore success, not an error to report.
fn seed_version_position(backend: Arc<dyn KvBackend>) -> Result<()> {
    let version_key = VersionPositionKey.encode();
    if backend
        .get(VersionPositionKey::keyspace(), &version_key)?
        .is_some()
    {
        return Ok(());
    }
    // The store home, because a store written before the log was partitioned
    // had exactly one log and this is where the migration files it.
    let applied_key = AppliedPositionKey::new(LogId::unattributed(Reach::Store)).encode();
    let applied = match backend.get(AppliedPositionKey::keyspace(), &applied_key)? {
        Some(value) => Sequence::decode(value.as_slice())?,
        None => Sequence::ZERO,
    };
    let batch = WriteBatch::new()
        .expect_absent(VersionPositionKey::keyspace(), version_key.clone())
        .put(
            VersionPositionKey::keyspace(),
            version_key,
            applied.encode(),
        );
    match backend.apply(batch) {
        Ok(()) | Err(tessari_kv::Error::Conflict { .. }) => Ok(()),
        Err(other) => Err(other.into()),
    }
}

#[cfg(test)]
mod tests {
    // Test assertions are exactly where a panic is the correct outcome.
    #![allow(clippy::panic, clippy::unwrap_used)]

    use tessari_encoding::LogRecord;
    use tessari_kv::MemoryBackend;

    use super::*;
    use crate::error::Error;

    fn backend() -> Arc<dyn KvBackend> {
        Arc::new(MemoryBackend::new())
    }

    #[test]
    fn a_fresh_store_writes_its_format_and_starts_at_sequence_zero() {
        let store = Store::open(backend()).unwrap();
        assert_eq!(
            store
                .committed_tail(store.own_log(Reach::Store).unwrap())
                .unwrap(),
            Sequence::ZERO
        );
        assert!(store.logs().unwrap().is_empty(), "nothing written yet");
        assert_eq!(
            read_format_version(Arc::clone(store.backend())).unwrap(),
            Some(FormatVersion::CURRENT)
        );
    }

    #[test]
    fn a_fresh_store_starts_its_version_counter_at_zero_as_well() {
        let store = Store::open(backend()).unwrap();
        assert_eq!(store.committed_version().unwrap(), Sequence::ZERO);
    }

    #[test]
    fn a_store_opened_without_a_version_counter_resumes_it_from_the_applied_position() {
        let backend = backend();
        let store = Store::open(Arc::clone(&backend)).unwrap();
        // The shape a store written before the version was separated from the
        // log position has on disk: a position, and no counter beside it. Its
        // records were stamped at that position, so the position *is* the
        // version they hold.
        backend
            .apply(
                WriteBatch::new()
                    .put(
                        AppliedPositionKey::keyspace(),
                        AppliedPositionKey::new(LogId::unattributed(Reach::Store)).encode(),
                        Sequence::new(7).encode(),
                    )
                    .delete(VersionPositionKey::keyspace(), VersionPositionKey.encode()),
            )
            .unwrap();
        drop(store);

        let reopened = Store::open(backend).unwrap();
        assert_eq!(
            reopened.committed_version().unwrap(),
            Sequence::new(7),
            "a counter restarted at zero would reissue versions records already hold"
        );
    }

    #[test]
    fn reopening_a_store_does_not_rewrite_its_metadata() {
        let shared = backend();
        let first = Store::open(Arc::clone(&shared)).unwrap();
        drop(first);
        let second = Store::open(shared).unwrap();
        assert_eq!(
            read_format_version(Arc::clone(second.backend())).unwrap(),
            Some(FormatVersion::CURRENT)
        );
    }

    #[test]
    fn a_store_written_before_the_epoch_opens_and_reads_as_the_first_leadership() {
        // Version 2 must not orphan the stores version 1 already wrote. What the
        // move to version 3 changed is the second half of the old assertion:
        // an older store IS rewritten now, because its log keys no longer decode
        // at all and leaving them would be leaving the store unreadable rather
        // than leaving it alone.
        let shared = backend();
        Store::open(Arc::clone(&shared)).unwrap();
        shared
            .apply(WriteBatch::new().put(
                FormatVersionKey::keyspace(),
                FormatVersionKey.encode(),
                FormatVersion::new(1).encode(),
            ))
            .unwrap();

        let store = Store::open(Arc::clone(&shared)).unwrap();
        assert_eq!(
            read_format_version(Arc::clone(store.backend())).unwrap(),
            Some(FormatVersion::CURRENT),
            "an older log is given its home at open, and the version says so"
        );

        store
            .apply_record(
                Writer::UNATTRIBUTED,
                Sequence::new(1),
                &LogRecord::new(Vec::new()),
            )
            .unwrap();
        let (_, record) = store
            .log_records(LogId::unattributed(Reach::Store), Sequence::new(1), 1)
            .unwrap()
            .pop()
            .expect("the record just applied");
        assert_eq!(
            record.epoch(),
            tessari_types::Epoch::ZERO,
            "a build that elects nobody writes the first and only leadership"
        );
    }

    #[test]
    fn a_log_written_before_it_had_writers_is_rewritten_and_attributed_to_nobody() {
        // The second migration, end to end and against real bytes: a store
        // standing in the shape version 4 left — a homed log key and one
        // position per home, neither naming a writer — is opened, and every
        // record it held is readable afterwards at the position it held, in the
        // log nobody is recorded as having written.
        let shared = backend();
        Store::open(Arc::clone(&shared)).unwrap();
        let home = Reach::Database(
            tessari_types::NamespaceId::new(1),
            tessari_types::DatabaseId::new(2),
        );
        let mut batch = WriteBatch::new().put(
            FormatVersionKey::keyspace(),
            FormatVersionKey.encode(),
            FormatVersion::HOMED_LOG.encode(),
        );
        for sequence in 1_u64..=3 {
            // The old shape, written out here rather than built by a helper:
            // this test is the only thing left that knows it, which is the same
            // reason its predecessor above spells its own out.
            let mut key = vec![KeyKind::LogEntry.tag()];
            key.extend_from_slice(&unqualified_home(home));
            key.extend_from_slice(&sequence.to_be_bytes());
            batch = batch.put(
                LogKey::keyspace(),
                Key::from(key),
                LogRecord::new(Vec::new()).encode(),
            );
        }
        let mut position = vec![KeyKind::AppliedPosition.tag()];
        position.extend_from_slice(&unqualified_home(home));
        batch = batch.put(
            AppliedPositionKey::keyspace(),
            Key::from(position),
            Sequence::new(3).encode(),
        );
        shared.apply(batch).unwrap();

        let store = Store::open(Arc::clone(&shared)).unwrap();
        let migrated = LogId::unattributed(home);
        assert_eq!(
            store.logs().unwrap(),
            vec![migrated],
            "the log kept its home and was attributed to nobody, because \
             nothing recorded who wrote it"
        );
        assert_ne!(
            migrated,
            store.own_log(home).unwrap(),
            "and NOT to the node that happened to open the file — a follower's \
             log holds the records its leader wrote"
        );
        assert_eq!(store.committed_tail(migrated).unwrap(), Sequence::new(3));
        let positions: Vec<Sequence> = store
            .log_records(migrated, Sequence::new(1), 16)
            .unwrap()
            .into_iter()
            .map(|(sequence, _)| sequence)
            .collect();
        assert_eq!(
            positions,
            vec![Sequence::new(1), Sequence::new(2), Sequence::new(3)],
            "every record kept the position it held"
        );

        // Done once: a second open finds nothing of the old shape and leaves
        // the store exactly as the first left it.
        drop(store);
        let reopened = Store::open(shared).unwrap();
        assert_eq!(
            read_format_version(Arc::clone(reopened.backend())).unwrap(),
            Some(FormatVersion::CURRENT)
        );
        assert_eq!(reopened.logs().unwrap(), vec![migrated]);
        assert_eq!(
            reopened
                .log_records(migrated, Sequence::new(1), 16)
                .unwrap()
                .len(),
            3
        );
    }

    #[test]
    fn a_store_whose_history_predates_writers_does_not_report_itself_empty() {
        // Q-764, measured before it was written: a store written by
        // `0.0.6-beta` and opened by `0.3.0-beta` answered `--health` with
        // "well — committed to sequence 0" while `--backup` read every record
        // out of the same store. A store nobody has ever written answers that
        // same sentence, so the two states an operator most needs to tell apart
        // — nothing here, and everything here under a name this node does not
        // own — were one sentence, at the one moment an upgrade makes somebody
        // read it.
        //
        // The cause is not a lost record. `give_an_older_log_its_writer`
        // attributes what it rewrites to NOBODY, deliberately and correctly,
        // and this reports the node's OWN log, which is empty until this node
        // writes. Both halves are right and the sentence built from one of them
        // was not.
        let shared = backend();
        Store::open(Arc::clone(&shared)).unwrap();
        let home = Reach::Store;
        let mut batch = WriteBatch::new().put(
            FormatVersionKey::keyspace(),
            FormatVersionKey.encode(),
            FormatVersion::HOMED_LOG.encode(),
        );
        for sequence in 1_u64..=3 {
            let mut key = vec![KeyKind::LogEntry.tag()];
            key.extend_from_slice(&unqualified_home(home));
            key.extend_from_slice(&sequence.to_be_bytes());
            batch = batch.put(
                LogKey::keyspace(),
                Key::from(key),
                LogRecord::new(Vec::new()).encode(),
            );
        }
        let mut position = vec![KeyKind::AppliedPosition.tag()];
        position.extend_from_slice(&unqualified_home(home));
        batch = batch.put(
            AppliedPositionKey::keyspace(),
            Key::from(position),
            Sequence::new(3).encode(),
        );
        shared.apply(batch).unwrap();

        // Since ADR-0107 a log written before writers were named is the line's
        // log (`Writer::UNATTRIBUTED` is `Writer::LINE`), and health reports the
        // log this node's history is in — so the history is counted where it
        // is, and nothing is left "elsewhere" to need the second number.
        let held = Store::open(shared).unwrap().health().unwrap();
        assert_eq!(
            held.committed,
            Sequence::new(3),
            "the store is not empty, and the number an operator reads says so"
        );
        assert_eq!(held.elsewhere, None);
    }

    #[test]
    fn a_store_nobody_has_written_holds_nothing_elsewhere() {
        // The control the test above needs to mean anything: an empty store
        // must not acquire a second number, or `elsewhere` would report history
        // in every store there is and stop distinguishing anything.
        let held = Store::open(backend()).unwrap().health().unwrap();
        assert_eq!(held.committed, Sequence::ZERO);
        assert_eq!(held.elsewhere, None);
    }

    /// A home as the nine bytes a key carried it in before writers were named.
    ///
    /// Taken from the qualified encoding rather than written out by hand, which
    /// is exact: the writer is a fixed-width suffix, so the leading bytes of a
    /// qualified key ARE the unqualified one.
    fn unqualified_home(home: Reach) -> Vec<u8> {
        AppliedPositionKey::new(LogId::unattributed(home))
            .encode()
            .into_bytes()
            .get(1..1 + REACH_LEN)
            .unwrap()
            .to_vec()
    }

    #[test]
    fn a_log_written_before_it_had_homes_is_rewritten_into_the_store_home() {
        // The migration, end to end and against real bytes: a store standing in
        // the shape version 2 left — flat log keys and one singleton position —
        // is opened, and every record it held is readable afterwards at the
        // position it held, in the home one leader wrote it into.
        let shared = backend();
        Store::open(Arc::clone(&shared)).unwrap();
        let mut batch = WriteBatch::new().put(
            FormatVersionKey::keyspace(),
            FormatVersionKey.encode(),
            FormatVersion::new(2).encode(),
        );
        for sequence in 1_u64..=3 {
            // The old shape, written out here rather than built by a helper:
            // this test is the only thing left that knows it.
            let mut key = vec![KeyKind::LogEntry.tag()];
            key.extend_from_slice(&sequence.to_be_bytes());
            batch = batch.put(
                LogKey::keyspace(),
                Key::from(key),
                LogRecord::new(Vec::new()).encode(),
            );
        }
        batch = batch.put(
            AppliedPositionKey::keyspace(),
            Key::from(vec![KeyKind::AppliedPosition.tag()]),
            Sequence::new(3).encode(),
        );
        shared.apply(batch).unwrap();

        let store = Store::open(Arc::clone(&shared)).unwrap();
        assert_eq!(
            store.logs().unwrap(),
            vec![LogId::unattributed(Reach::Store)],
            "one leader wrote all of it, so the store's own log is its home — \
             and nobody recorded which node that leader was"
        );
        assert_eq!(
            store
                .committed_tail(LogId::unattributed(Reach::Store))
                .unwrap(),
            Sequence::new(3)
        );
        let positions: Vec<Sequence> = store
            .log_records(LogId::unattributed(Reach::Store), Sequence::new(1), 16)
            .unwrap()
            .into_iter()
            .map(|(sequence, _)| sequence)
            .collect();
        assert_eq!(
            positions,
            vec![Sequence::new(1), Sequence::new(2), Sequence::new(3)],
            "every record kept the position it held"
        );

        // And it is done once: a second open finds nothing of the old shape and
        // leaves the store exactly as the first left it.
        drop(store);
        let reopened = Store::open(shared).unwrap();
        assert_eq!(
            reopened
                .log_records(LogId::unattributed(Reach::Store), Sequence::new(1), 16)
                .unwrap()
                .len(),
            3
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

    #[test]
    fn a_store_holding_data_without_a_format_stamp_is_refused_and_left_alone() {
        let shared = backend();
        let record = tessari_kv::Key::from(vec![KeyKind::Record.tag(), 1, 2, 3]);
        shared
            .apply(WriteBatch::new().put(
                tessari_kv::Keyspace::DATA,
                record.clone(),
                tessari_kv::Value::from(vec![1]),
            ))
            .unwrap();

        let error = Store::open(Arc::clone(&shared)).unwrap_err();
        assert_eq!(error.code(), "corruption", "{error}");
        match &error {
            Error::Encoding(tessari_encoding::Error::UnstampedStore { keyspace }) => {
                assert_eq!(*keyspace, "data");
            }
            other => panic!("unexpected error: {other}"),
        }
        assert_eq!(
            read_format_version(Arc::clone(&shared)).unwrap(),
            None,
            "not stamped on the way to refusing"
        );
        assert!(
            shared
                .get(tessari_kv::Keyspace::DATA, &record)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn an_empty_store_is_a_new_one_and_is_stamped() {
        let shared = backend();
        Store::open(Arc::clone(&shared)).unwrap();
        assert_eq!(
            read_format_version(shared).unwrap(),
            Some(FormatVersion::CURRENT)
        );
    }
}
