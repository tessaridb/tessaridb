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
mod format;
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
mod upgrading;
use upgrading::{
    give_an_older_log_its_home, give_an_older_log_its_writer, read_format_version,
    refuse_data_without_a_stamp, seed_version_position, write_initial_metadata,
};

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

#[cfg(test)]
mod tests;
