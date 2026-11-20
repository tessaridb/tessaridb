//! The source side of a cross-cluster link: reading what a link ships.
//!
//! A declared replication ([`crate::catalog::ReplicationDefinition`]) names one
//! database and the endpoint of another cluster. This module is the machinery
//! that serves it: read the database's logs after the position the far side
//! last confirmed, filter each record to the link's reach the way a follower's
//! collect is filtered ([`Store::log_records_within`] — the filter is on this
//! side, deliberately; a confidentiality model in which the party being
//! restricted applies the restriction is the one this store refuses
//! everywhere else), annotate the batch with its provenance, and hand it to a
//! sink.
//!
//! # Provenance rides the envelope, never the log
//!
//! The envelope carries the origin cluster's identity and the positions the
//! batch was read at, so the far side can apply by (origin, position, record
//! identity, version) — idempotent by construction, for the reason the
//! restore path already is: re-applying a batch is a no-op, not a divergence.
//! Nothing of this is written into the records; identity stays out of the log
//! (ADR-0018), and what the far side needs to place a batch travels with it.
//!
//! # Confirmed positions are a live relationship, and stay in memory
//!
//! Like followers and collections, what the far side has confirmed is a fact
//! about a live relationship: held in [`Links`], never persisted. A restarted
//! source re-learns the confirmed positions from the far side when the wire
//! handshake lands; until then the registry is the truth within one process
//! lifetime, and at-least-once is bounded at the confirmation point — a batch
//! is kept and re-read until the sink confirms it.

use std::collections::BTreeMap;
use std::sync::Arc;

use dashmap::DashMap;
use tessari_encoding::{LogId, LogRecord};
use tessari_types::Sequence;

use crate::catalog::{Catalog, Reach, ReplicationDefinition};
use crate::error::Result;
use crate::store::Store;

/// How many records one read of a link's log asks for.
const BATCH: usize = 256;

/// One batch, as read from one log for one link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Envelope {
    /// The cluster that wrote what the batch carries — this node's identity,
    /// which is the identity the far side bound the link's token to.
    pub origin: [u8; 16],
    /// The log the records were read from. Positions count in one log only,
    /// so a batch never mixes them.
    pub log: LogId,
    /// The position the read started at — one past the last confirmed record,
    /// so the far side can place the batch's first record exactly.
    pub from: Sequence,
    /// The records read, in log order, every sequence present (a record the
    /// reach filtered to nothing arrives empty, for the reason
    /// [`Store::log_records_within`] preserves them: positions are arithmetic,
    /// not suggestions).
    pub records: Vec<(Sequence, LogRecord)>,
}

/// What the far side has confirmed of one link: per log, the position it has
/// durably applied through. In memory for the reason the module header gives;
/// shared like the follower registries are. A DashMap for the same reason the
/// other registries shard: readers and writers of different links never
/// serialize on each other. Guards here are short and synchronous — copied
/// out before any suspension point, never held across one.
#[derive(Debug, Default)]
pub struct Links {
    confirmed: DashMap<String, BTreeMap<LogId, Sequence>>,
}

impl Links {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The confirmed position of `log` under `link`, or `start` when nothing
    /// has been confirmed — reads begin one past it.
    #[must_use]
    pub fn confirmed(&self, link: &str, log: LogId, start: Sequence) -> Sequence {
        self.confirmed
            .get(link)
            .and_then(|logs| logs.get(&log).copied())
            .unwrap_or(start)
    }

    /// Record that `link`'s far side has durably applied `log` through `to`.
    pub fn advance(&self, link: &str, log: LogId, to: Sequence) {
        self.confirmed
            .entry(link.to_owned())
            .and_modify(|logs| {
                let slot = logs.entry(log).or_insert(to);
                *slot = (*slot).max(to);
            })
            .or_insert_with(|| BTreeMap::from([(log, to)]));
    }

    /// The positions a link's far side may still ask for: per log, the
    /// confirmed position — the floor reads resume from. Holding these against
    /// pruning (`Store::hold_logs`) is what keeps at-least-once true under
    /// retention: a record the far side has not confirmed must survive until
    /// it has.
    #[must_use]
    pub fn floors(&self, link: &str) -> Vec<(LogId, Sequence)> {
        self.confirmed
            .get(link)
            .map(|logs| logs.iter().map(|(log, at)| (*log, *at)).collect())
            .unwrap_or_default()
    }

    /// Forget a link's positions (`DROP REPLICATION`).
    pub fn drop_link(&self, link: &str) {
        self.confirmed.remove(link);
    }
}

/// Where a link's envelopes go. The one implementation in this wave is an
/// in-process channel for the suite; the paired-link wire is the same seam —
/// the bytes leave here, and the transport swap does not touch the driver.
pub trait Sink {
    /// Ship one batch and answer the position durably applied through — the
    /// confirmation the at-least-once boundary is drawn at.
    ///
    /// Desugared to `impl Future + Send` rather than `async fn` so the bound a
    /// spawned driver task needs is named here, not inferred away.
    ///
    /// # Errors
    ///
    /// Returns an error when the far side cannot be reached or refuses the
    /// batch; the batch is kept and re-read on the next pass.
    fn ship(
        &mut self,
        link: &str,
        envelope: Envelope,
    ) -> impl std::future::Future<Output = Result<Sequence>> + Send;
}

/// Serve one link for one pass: read what the far side has not confirmed,
/// ship it, advance. The caller paces the loop, which keeps the backpressure
/// decision (and its test) at the caller; nothing here blocks on the far side
/// beyond the one ship await.
///
/// A caller that serves a link over time holds its floors against pruning for
/// the serving lifetime — `store.hold_logs(&registry.floors(&name))` — the
/// same promise a leader makes a follower while it copies (ADR-0094 D3), made
/// here for the same reason: a record pruned before the far side confirms it
/// is a batch that can never ship, and at-least-once is only a claim while
/// the record survives to be re-read.
///
/// Returns how many envelopes were shipped this pass.
///
/// # Errors
///
/// Returns an error when a log cannot be read or the sink keeps refusing.
pub async fn serve_once(
    store: &Store,
    definition: &ReplicationDefinition,
    registry: &Links,
    sink: &mut impl Sink,
) -> Result<usize> {
    let origin = store.node_identity().await?.id;
    let mut shipped = 0_usize;
    for (log, _) in logs_of(store, definition.database).await? {
        let start = store.log_start(log).await?;
        // A seeded link serves the tail from one past the seed position — in
        // the log the position counts in, the link's own. The parent logs
        // the reach reads re-read from their origins: the copy's already-
        // held positions answer them as no-ops, the at-least-once price the
        // intake's identity check already pays.
        let start = match definition.seed_at {
            Some(floor) if log.home == definition.database => start.max(floor),
            _ => start,
        };
        let confirmed = registry.confirmed(&definition.name, log, start);
        let from = Sequence::new(confirmed.get().max(start.get()).saturating_add(1));
        let records = store
            .log_records_within(definition.database, log, from, BATCH)
            .await?;
        if records.is_empty() {
            continue;
        }
        let envelope = Envelope {
            origin,
            log,
            from,
            records,
        };
        let confirmed = sink.ship(&definition.name, envelope).await?;
        registry.advance(&definition.name, log, confirmed);
        // The lifecycle's automatic half, on the serving side: once the far
        // side has confirmed through the seed position, the link is caught
        // up and reports standby.
        promote_when_caught_up(store, &definition.name, confirmed).await?;
        shipped = shipped.saturating_add(1);
    }
    Ok(shipped)
}

/// The automatic `seeding → standby` transition: a seeded link whose side of
/// the relationship holds `applied` through the seed position is caught up,
/// and the row says so. Amends the row in its own transaction when the
/// transition fires; a no-op for the unseeded, the already-promoted, and the
/// still-catching-up. `applied` is the local fact each caller already holds —
/// the confirmed position a serve just advanced, or the tail an intake just
/// landed.
///
/// # Errors
///
/// Returns an error when the row cannot be read or written; a transition
/// failure never turns the serve or the intake that noticed it into a
/// failure of its own.
async fn promote_when_caught_up(store: &Store, link: &str, applied: Sequence) -> Result<()> {
    let definition = {
        let mut reading = store.begin().await?;
        Catalog::new(&mut reading)
            .replications()
            .await?
            .into_iter()
            .find(|held| held.name == link)
    };
    let Some(definition) = definition else {
        return Ok(());
    };
    let Some(seed_at) = definition.seed_at else {
        return Ok(());
    };
    if definition.state != crate::ReplicationState::Seeding || applied < seed_at {
        return Ok(());
    }
    let mut transaction = store.begin().await?;
    let amended = Catalog::new(&mut transaction)
        .amend_replication(link, |row| row.state = crate::ReplicationState::Standby)
        .await?;
    if amended {
        transaction.commit().await?;
    }
    Ok(())
}

/// The logs a link's reach reads: those whose home is inside the reach, or
/// which contain it carrying the definitions it needs — the rule a state read
/// at this reach follows (`crate::state`), so the link's stream and a snapshot
/// at the same reach agree on what exists.
async fn logs_of(store: &Store, reach: Reach) -> Result<Vec<(LogId, Sequence)>> {
    let mut found = Vec::new();
    for log in store.logs().await? {
        if reach.contains(log.home) || log.home.contains(reach) {
            found.push((log, store.log_start(log).await?));
        }
    }
    Ok(found)
}

/// A sink that hands envelopes to a shared queue — the in-process transport
/// the suite drives. The far side in these tests is a queue the assertions
/// drain; the confirmation answers the batch's last record, which is what a
/// durable apply through that position means.
#[derive(Debug, Clone, Default)]
pub struct ChannelSink {
    queue: Arc<std::sync::Mutex<Vec<(String, Envelope)>>>,
}

impl ChannelSink {
    /// An empty sink.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Everything shipped so far, drained.
    pub fn drain(&self) -> Vec<(String, Envelope)> {
        std::mem::take(
            &mut *self
                .queue
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        )
    }
}

impl Sink for ChannelSink {
    fn ship(
        &mut self,
        link: &str,
        envelope: Envelope,
    ) -> impl std::future::Future<Output = Result<Sequence>> + Send {
        let confirmed = envelope
            .records
            .last()
            .map_or(envelope.from, |(sequence, _)| *sequence);
        self.queue
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push((link.to_owned(), envelope));
        std::future::ready(Ok(confirmed))
    }
}

/// The databases this cluster receives from another, and the inbound link each
/// is received over. Derived from the catalog's replication rows on every
/// commit and apply, the way record counts are — a follower replaying the
/// record reaches the same registry, so the gate below it answers identically
/// on every node. Sharded like the other registries: a commit touching one
/// database never waits on a link it has nothing to do with.
#[derive(Debug, Default)]
pub struct Replicated {
    inbound: DashMap<(tessari_types::NamespaceId, tessari_types::DatabaseId), String>,
}

impl Replicated {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The link a database is received over, when it is received.
    #[must_use]
    pub fn link_over(
        &self,
        tenancy: (tessari_types::NamespaceId, tessari_types::DatabaseId),
    ) -> Option<String> {
        self.inbound.get(&tenancy).map(|held| held.clone())
    }

    /// Learn what one record's mutations say about inbound links. Called from
    /// the commit and the apply in the same order as every other derived
    /// thing, for the same reason: a state reached by replay derives the same
    /// registry as the state that was committed.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CatalogMalformed`] when a replication row in the log
    /// cannot be decoded — refused rather than skipped, because a row read as
    /// nothing is a gate that quietly stops refusing.
    pub(crate) fn maintain(&self, record: &tessari_encoding::LogRecord) -> Result<()> {
        use tessari_encoding::RecordValue;
        for mutation in record.mutations() {
            if mutation.namespace != crate::catalog::system::SYSTEM_NAMESPACE
                || mutation.database != crate::catalog::system::SYSTEM_DATABASE
                || mutation.table != crate::catalog::system::REPLICATIONS
            {
                continue;
            }
            match mutation.value.value() {
                RecordValue::Present(payload) => {
                    let definition = ReplicationDefinition::from_value(
                        &tessari_encoding::decode_payload(payload)?,
                    )?;
                    if definition.inbound
                        && let Reach::Database(namespace, database) = definition.database
                    {
                        self.inbound
                            .insert((namespace, database), definition.name.clone());
                    }
                }
                RecordValue::Tombstone => {
                    // A dropped link forgets its databases; the tombstone's id
                    // is the link name the row lived under.
                    if let tessari_types::RecordId::Text(name) = &mutation.id {
                        self.inbound.retain(|_, link| link != name);
                    }
                }
            }
        }
        Ok(())
    }
}

/// The replica side of a link: apply what a shipped envelope carries, at the
/// positions it was read from. The records ride the ordinary apply a follower
/// replays a collect by (D6), so the copy keeps the source's ids, the log the
/// source wrote, and the persistence both already have — a position applied is
/// the position persisted, and a batch applied twice is a no-op the second
/// time, not a divergence.
///
/// Returns the position applied through — the confirmation the source's
/// at-least-once boundary is drawn at.
///
/// # Errors
///
/// Returns an error when the batch cannot be applied — a gap where a position
/// was pruned, a divergence from a record the copy already holds, or a
/// substrate failure. The source keeps the batch and re-reads it when the
/// refusal is retryable; a divergence is not, and is the operator's question.
/// A shipped record the reach emptied to nothing is a position marker, and at
/// a position the copy already holds it is a no-op, not a divergence: on a
/// clustered receiving side the coarser logs ride the same line log every
/// cluster writes its own local rows into — membership, leadership renewals —
/// so at one position the copy holds its own unfiltered record while the
/// shipment offers the source's record filtered to nothing. Both say the
/// reach has no content at this position; the copy's own record stands. The
/// empty marker still extends the log at a fresh position, because positions
/// are arithmetic; a record with mutations keeps the full divergence check.
///
/// Returns the position applied through — the confirmation the source's
/// at-least-once boundary is drawn at.
///
/// # Errors
///
/// Returns an error when the batch cannot be applied — a gap where a position
/// was pruned, a divergence from a record the copy already holds, or a
/// substrate failure. The source keeps the batch and re-reads it when the
/// refusal is retryable; a divergence is not, and is the operator's question.
pub async fn intake(store: &Store, envelope: Envelope) -> Result<Sequence> {
    let mut applied = envelope.from;
    for (at, record) in envelope.records {
        if record.mutations().is_empty()
            && at.get() <= store.committed_tail(envelope.log).await?.get()
        {
            applied = at;
            continue;
        }
        store.apply_shipped(envelope.log, at, &record).await?;
        applied = at;
    }
    // The automatic half of the lifecycle, on the receiving side: the copy
    // this envelope extends belongs to the inbound link the envelope's log
    // names; once the copy's tail reaches the seed position, the link is
    // caught up and reports standby.
    if let Reach::Database(namespace, database) = envelope.log.home
        && let Some(link) = store.replicated().link_over((namespace, database))
    {
        promote_when_caught_up(store, &link, store.committed_tail(envelope.log).await?).await?;
    }
    Ok(applied)
}

/// Serve every declared outbound link until `stop`, one pass per tick — the
/// runtime shape the node wraps in a supervised task, and the shape the
/// two-cluster suite drives directly.
///
/// # Live pause
///
/// The catalog is re-read every pass, so `ALTER REPLICATION … PAUSED` holds
/// the stream at the next tick rather than at the next restart — a stopped
/// stream is a statement, not a process to hunt down. The same re-read means
/// a `DROP REPLICATION` retires a link between one pass and the next, and a
/// newly declared one starts serving without anything being told.
///
/// # What a refusal does
///
/// Nothing, for the reason a collect's refusal does nothing: a far side that
/// is down leaves the registry where it was, is logged, and is asked again
/// next pass. At-least-once holds because the unconfirmed batch survives —
/// the hold below is what makes "survives" true under pruning.
///
/// # The hold
///
/// Each pass holds the link's confirmed floors against pruning for the
/// pass's duration. Between passes a record is not held, which is honest:
/// the promise is made while serving, and a link nobody is serving has no
/// live relationship for the registry to describe either.
///
/// # The persisted cursor
///
/// A row's `position` field is the cross-restart cache of the live truth: on
/// the first pass the registry is seeded from every outbound row that holds
/// one (the far side re-teaches whatever lies past it — a position at or
/// below the copy's tail is a no-op on identical bytes), and on stop every
/// active link's floor is written back, so a restart resumes near where the
/// relationship stood instead of re-shipping from the origin.
///
/// # Errors
///
/// Returns an error only when the catalog cannot be read; per-link failures
/// are logged and the pass goes on.
pub async fn serve_links(
    store: &Store,
    sink: &mut impl Sink,
    mut stop: tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    seed_from_rows(store).await?;
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(20));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            biased;
            changed = stop.changed() => {
                // The sender dropped or set the flag: either way, stop — and
                // land the cursors first, while the store is still open.
                if changed.is_err() || *stop.borrow() {
                    persist_floors(store).await?;
                    return Ok(());
                }
            }
            _ = tick.tick() => {}
        }
        let links = {
            let mut reading = store.begin().await?;
            Catalog::new(&mut reading).replications().await?
        };
        for definition in links.iter().filter(|held| !held.inbound && !held.paused) {
            let _hold = store.hold_logs(&store.links().floors(&definition.name));
            if let Err(failure) = serve_once(store, definition, store.links(), sink).await {
                tracing::warn!(
                    link = %definition.name,
                    error = %failure,
                    "a link's pass failed; its unconfirmed batch survives for the next"
                );
            }
        }
    }
}

/// Seed the registry from every outbound row that persisted a position: the
/// cursor the last run of this relationship left, read as the floor the link's
/// own log resumes from.
async fn seed_from_rows(store: &Store) -> Result<()> {
    let links = {
        let mut reading = store.begin().await?;
        Catalog::new(&mut reading).replications().await?
    };
    for definition in links.iter().filter(|held| !held.inbound) {
        // The cursor and the seed floor, whichever stands further along: a
        // seeded link resumes from the snapshot's end even when no serve has
        // confirmed past it yet. A row with neither stays unserved — nothing
        // ships, and the row's `None` cursor keeps meaning the driver never
        // shipped from it.
        let Some(floor) = definition.position.max(definition.seed_at) else {
            continue;
        };
        if let Some(log) = own_log(store, definition.database).await? {
            store.links().advance(&definition.name, log, floor);
        }
    }
    Ok(())
}

/// Write every active link's floor back to its row, so a restart resumes near
/// where the relationship stood. The far side's applied tail remains the
/// durable truth — this cursor only saves it the re-teach.
async fn persist_floors(store: &Store) -> Result<()> {
    let links = {
        let mut reading = store.begin().await?;
        Catalog::new(&mut reading).replications().await?
    };
    for definition in links.iter().filter(|held| !held.inbound) {
        let Some(log) = own_log(store, definition.database).await? else {
            continue;
        };
        if !store
            .links()
            .floors(&definition.name)
            .iter()
            .any(|(held, _)| *held == log)
        {
            continue;
        }
        let floor = store
            .links()
            .confirmed(&definition.name, log, Sequence::new(0));
        if definition.position == Some(floor) {
            continue;
        }
        let mut transaction = store.begin().await?;
        let amended = Catalog::new(&mut transaction)
            .amend_replication(&definition.name, |row| row.position = Some(floor))
            .await?;
        if amended {
            transaction.commit().await?;
        }
    }
    Ok(())
}

/// The one log whose home is the reach itself — the log a link's position
/// counts in, beside the parent logs whose records the reach filter empties
/// but whose positions still ride the stream.
async fn own_log(store: &Store, reach: Reach) -> Result<Option<LogId>> {
    Ok(logs_of(store, reach)
        .await?
        .into_iter()
        .map(|(log, _)| log)
        .find(|log| log.home == reach))
}
