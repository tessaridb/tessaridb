//! Starting what the catalog declares, and stopping it cleanly.
//!
//! # One OS thread per consumer, and no runtime
//!
//! This node carries no async runtime, deliberately (G007): the store below is
//! synchronous, and an async server over a synchronous store is a thread pool
//! wearing a runtime's clothes. A consumer is a loop that blocks on a broker and
//! then blocks on a commit, which is the shape a thread is for.
//!
//! ADR-0024 measured that the chosen client has a synchronous surface, so this
//! is a decision supported by evidence rather than a preference held despite it.
//!
//! # The loop, and the order inside it
//!
//! ```text
//! poll a batch          — nothing written, offset unmoved → clean redelivery
//! shape each message    — as above
//! COMMIT the store      — data present, offset unmoved → duplicate on restart
//! commit the offset     — consistent
//! ```
//!
//! The third and fourth lines are the delivery guarantee. A crash between them
//! redelivers the batch, and the batch re-applies to the same records because
//! every record's identity comes from the message. That is at-least-once with
//! idempotent application, and it is not exactly-once.
//!
//! # Why a batch is not atomic, and does not need to be
//!
//! A batch is written in one transaction because that is cheaper, not because it
//! must be. If a process dies with half a batch applied, the offset has not
//! moved, so the whole batch is redelivered and the applied half is written
//! again to the same identities. Atomicity would buy nothing that idempotence
//! does not already give.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use tessari_session::Session;
use tessari_storage::{Catalog, ConsumerDefinition, OnFailure, Store};
use tessari_types::{RecordId, Value};

use crate::apply::shape;
use crate::source::{Source, SourceError};

/// How long one poll waits for a message before the loop checks whether it has
/// been told to stop.
///
/// Short enough that a shutdown is not something an operator waits on, long
/// enough that an idle consumer is not a spin loop. It bounds shutdown latency
/// and nothing else — a message arriving during it is returned immediately.
const POLL: Duration = Duration::from_millis(200);

/// How many messages one transaction carries at most.
///
/// A bound rather than a tuning knob: without one, a consumer catching up on a
/// backlog builds a transaction the size of the backlog and the process runs out
/// of memory instead of falling behind.
const BATCH: usize = 500;

/// How many times a quarantining consumer retries before parking a message.
///
/// Retries are for a *transient* failure — the store refusing a write under
/// contention — and not for a malformed payload, which will fail identically
/// every time. So the count is small: its job is to ride out a blip, not to
/// spend minutes on a message that can never be applied.
const RETRIES: u32 = 3;

/// Somewhere to open a consumer's messages from.
///
/// A factory rather than a source, because the runner starts `parallelism`
/// consumers per declaration and each needs its own connection to the group.
pub trait Broker: Send + Sync {
    /// Open one consumer against this declaration.
    ///
    /// # Errors
    ///
    /// Returns [`SourceError`] when the connection could not be made.
    fn open(&self, definition: &ConsumerDefinition) -> Result<Box<dyn Source>, SourceError>;
}

/// The consumers this process started, and the way to stop them.
///
/// Dropping this **stops and joins** them. That is deliberate: a handle whose
/// drop left threads running would make "the node is shutting down" a thing the
/// caller has to remember to say, and forgetting it abandons an in-flight batch
/// — which turns at-least-once into a lie about which end.
#[derive(Debug)]
pub struct Started {
    stopping: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
}

impl Started {
    /// Stop every consumer and wait for it to finish the batch it is in.
    ///
    /// Idempotent, so the explicit call and the drop below do not fight.
    pub fn stop(&mut self) {
        self.stopping.store(true, Ordering::Relaxed);
        for thread in self.threads.drain(..) {
            // A consumer thread that panicked has already reported it; there is
            // nothing here to do about it that is not making shutdown fail.
            drop(thread.join());
        }
    }

    /// How many consumer threads are running.
    #[must_use]
    pub fn threads(&self) -> usize {
        self.threads.len()
    }

    /// The flag that tells every consumer to stop taking new messages.
    ///
    /// Handed out so that a shutdown **sequencer** can set it at the stage it
    /// belongs to, rather than only at the moment this handle is dropped. Those
    /// are not the same instant: a node that has said *not ready* and refused new
    /// connections is still ingesting until somebody says otherwise, and a
    /// consumer writing after the node stopped serving is a writer nothing is
    /// waiting for.
    ///
    /// Setting it does **not** join. Each consumer finishes the batch it is in —
    /// writing it and committing its offset — and then returns, so the join is a
    /// separate step and [`Started::stop`] is still what performs it.
    #[must_use]
    pub fn halting(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.stopping)
    }
}

impl Drop for Started {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Starts what the catalog declares.
#[derive(Debug)]
pub struct Runner;

impl Runner {
    /// Start every declared consumer, and answer with the handle that stops them.
    ///
    /// Called at open and after a restart. A declaration whose destination has
    /// gone, or whose connection cannot be made, is **logged and skipped** rather
    /// than failing the start: one broken consumer must not stop a node from
    /// serving, and `INFO FOR CONSUMER` reports it as not running here.
    ///
    /// # Errors
    ///
    /// Returns the store's failure when the catalog cannot be read at all.
    pub fn start(store: &Store, broker: &Arc<dyn Broker>) -> Result<Started, String> {
        let declared = declarations(store)?;
        let stopping = Arc::new(AtomicBool::new(false));
        let mut threads = Vec::new();
        for (definition, table) in declared {
            for index in 0..definition.parallelism {
                let source = match broker.open(&definition) {
                    Ok(source) => source,
                    Err(failure) => {
                        log::warn!(
                            "consumer {} could not be opened: {failure}",
                            definition.name
                        );
                        store.running().stopped(&definition.name);
                        break;
                    }
                };
                if index == 0 {
                    store.running().started(&definition.name);
                }
                let running = Consuming {
                    store: store.clone(),
                    definition: definition.clone(),
                    table: table.clone(),
                    stopping: Arc::clone(&stopping),
                };
                let named = format!("consumer:{}#{index}", definition.name);
                match std::thread::Builder::new()
                    .name(named)
                    .spawn(move || running.run(source))
                {
                    Ok(thread) => threads.push(thread),
                    Err(failure) => {
                        log::warn!(
                            "consumer {} could not be started: {failure}",
                            definition.name
                        );
                    }
                }
            }
        }
        Ok(Started { stopping, threads })
    }
}

/// Every declaration, with the statement text its destination is written as.
///
/// The destination is resolved to **names** here, once, rather than per batch:
/// the runner writes through the language, and the only text it composes comes
/// from the catalog. No byte of any message ever reaches a statement.
fn declarations(store: &Store) -> Result<Vec<(ConsumerDefinition, Destination)>, String> {
    let mut transaction = store.begin().map_err(|failure| failure.to_string())?;
    let declared = Catalog::new(&mut transaction)
        .consumers()
        .map_err(|failure| failure.to_string())?;
    let mut found = Vec::new();
    for definition in declared {
        match destination_of(&mut transaction, &definition) {
            Some(table) => found.push((definition, table)),
            None => log::warn!(
                "consumer {} has no destination any more, so it is not started",
                definition.name
            ),
        }
    }
    transaction.rollback();
    Ok(found)
}

/// Where a consumer's records go, named as a statement would name it.
#[derive(Debug, Clone)]
struct Destination {
    namespace: String,
    database: String,
    table: String,
}

/// Resolve a declaration's ids back to the names a statement uses.
fn destination_of(
    transaction: &mut tessari_storage::Transaction<'_>,
    definition: &ConsumerDefinition,
) -> Option<Destination> {
    let catalog = Catalog::new(transaction);
    let namespace = catalog
        .namespaces()
        .ok()?
        .into_iter()
        .find(|held| held.id == definition.namespace)?
        .name;
    let database = catalog
        .databases_in(definition.namespace)
        .ok()?
        .into_iter()
        .find(|held| held.id == definition.database)?
        .name;
    let table = catalog
        .tables_in(definition.namespace, definition.database)
        .ok()?
        .into_iter()
        .find(|held| held.id == definition.destination)?
        .name;
    // Every one of these came from a `DEFINE` statement, so it is already an
    // identifier — but it is checked rather than trusted, because this is the
    // only text this crate composes and the check costs nothing.
    if !plain(&namespace) || !plain(&database) || !plain(&table) {
        return None;
    }
    Some(Destination {
        namespace,
        database,
        table,
    })
}

/// Whether a name can stand unquoted in a statement.
fn plain(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with(|first: char| first.is_ascii_digit())
        && name
            .chars()
            .all(|held| held.is_ascii_alphanumeric() || held == '_')
}

/// One consumer thread's world.
struct Consuming {
    store: Store,
    definition: ConsumerDefinition,
    table: Destination,
    stopping: Arc<AtomicBool>,
}

impl Consuming {
    /// Read, shape, write, commit — until told to stop.
    fn run(self, mut source: Box<dyn Source>) {
        while !self.stopping.load(Ordering::Relaxed) {
            match self.once(source.as_mut()) {
                Ok(()) => {}
                Err(failure) => {
                    log::error!("consumer {} stopped: {failure}", self.definition.name);
                    self.store
                        .running()
                        .advanced(&self.definition.name, |progress| {
                            progress.last_error = Some(failure.clone());
                        });
                    // `stop` halts this consumer and leaves the rest alone. The
                    // registry entry goes, so `INFO FOR CONSUMER` reports it as
                    // not running here rather than as running and stuck.
                    self.store.running().stopped(&self.definition.name);
                    return;
                }
            }
        }
        self.store.running().stopped(&self.definition.name);
    }

    /// One batch: poll, shape, write, then move the offset.
    fn once(&self, source: &mut dyn Source) -> Result<(), String> {
        let mut batch = Vec::new();
        while batch.len() < BATCH {
            match source.poll(POLL) {
                Ok(Some(message)) => batch.push(message),
                Ok(None) => break,
                Err(failure) => return Err(failure.to_string()),
            }
            if self.stopping.load(Ordering::Relaxed) {
                break;
            }
        }
        if batch.is_empty() {
            return Ok(());
        }

        let mut records = Vec::new();
        let mut parked = 0_u64;
        for message in &batch {
            match shape(&message.payload, &self.definition) {
                Ok(shaped) => records.push(shaped),
                Err(why) => match self.definition.on_failure {
                    // Halting is the whole point of `stop`: an operator who chose
                    // it wants to look at the message before anything else moves.
                    OnFailure::Stop => {
                        return Err(format!(
                            "message at partition {} offset {} could not be applied: {why}",
                            message.partition, message.offset
                        ));
                    }
                    OnFailure::Quarantine => {
                        // Bounded, findable, and it does not stall the partition
                        // — which is the failure this policy exists to avoid, and
                        // the one whose absence stalls every other consumer in
                        // the group through a rebalance.
                        log::warn!(
                            "consumer {} quarantined partition {} offset {}: {why}",
                            self.definition.name,
                            message.partition,
                            message.offset
                        );
                        parked = parked.saturating_add(1);
                    }
                },
            }
        }

        let applied = u64::try_from(records.len()).unwrap_or(u64::MAX);
        // **The store first.** A crash between here and the offset commit
        // redelivers the batch, which re-applies to the same identities.
        self.write(&records)?;

        // And only then the offset. A failure here is not fatal: the batch is
        // already durable and will be redelivered, which is the direction this
        // design chose.
        if let Err(failure) = source.commit() {
            log::warn!(
                "consumer {} wrote its batch but could not commit the offset: {failure}",
                self.definition.name
            );
        }

        let positions: Vec<(i32, i64)> = batch
            .iter()
            .map(|message| (message.partition, message.offset))
            .collect();
        self.store
            .running()
            .advanced(&self.definition.name, |progress| {
                progress.applied = progress.applied.saturating_add(applied);
                progress.quarantined = progress.quarantined.saturating_add(parked);
                for (partition, offset) in positions {
                    progress.positions.insert(partition, offset);
                }
            });
        Ok(())
    }

    /// Write a batch through the language, in one transaction.
    ///
    /// `SET` rather than `CREATE`, because a redelivered message must land on the
    /// record it landed on last time — that replacement **is** the idempotence
    /// the delivery guarantee rests on, and `CREATE` would refuse the second
    /// delivery while `UPDATE` would refuse the first.
    ///
    /// Every value travels as a **parameter**. No byte of any message reaches
    /// the statement text, so a payload holding a quote is a payload holding a
    /// quote and not a statement.
    fn write(&self, records: &[crate::apply::Shaped]) -> Result<(), String> {
        if records.is_empty() {
            return Ok(());
        }
        let mut script = format!(
            "USE NAMESPACE {}; USE DATABASE {}; BEGIN;",
            self.table.namespace, self.table.database
        );
        let mut parameters = tessari_session::Parameters::new();
        for (at, shaped) in records.iter().enumerate() {
            script.push_str(&format!(" SET {}:$id{at} = $rec{at};", self.table.table));
            parameters.insert(format!("id{at}"), identity_value(&shaped.id));
            parameters.insert(format!("rec{at}"), shaped.record.clone());
        }
        script.push_str(" COMMIT;");

        let mut attempt = 0_u32;
        loop {
            let mut session = Session::new(&self.store);
            match session.run_with(&script, &parameters) {
                Ok(_) => return Ok(()),
                Err(failure) => {
                    attempt = attempt.saturating_add(1);
                    if attempt > RETRIES {
                        return Err(format!("the batch could not be written: {failure}"));
                    }
                    log::warn!(
                        "consumer {} retrying a batch ({attempt}/{RETRIES}): {failure}",
                        self.definition.name
                    );
                }
            }
        }
    }
}

/// A record identity, as the value a parameter carries.
fn identity_value(id: &RecordId) -> Value {
    match id {
        RecordId::Int(held) => Value::Number(tessari_types::Number::Integer(*held)),
        RecordId::Text(held) => Value::from(held.as_str()),
        // Every other identity kind is one this crate never produces — `shape`
        // answers with an integer or a string and nothing else — so this arm is
        // unreachable rather than a fallback with meaning.
        other => Value::from(other.to_string().as_str()),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]

    use super::*;

    #[test]
    fn a_name_that_could_not_stand_in_a_statement_is_refused() {
        // The only text this crate composes is table and tenancy names read back
        // out of the catalog. They came from `DEFINE` statements, so they are
        // already identifiers — this is the check that says so rather than
        // assumes it.
        assert!(plain("orders"));
        assert!(plain("orders_2026"));
        assert!(!plain(""));
        assert!(!plain("2026_orders"));
        assert!(!plain("orders; DROP TABLE users"));
        assert!(!plain("orders-live"));
    }

    #[test]
    fn an_identity_travels_as_a_value_and_not_as_text() {
        assert_eq!(
            identity_value(&RecordId::Int(7)),
            Value::Number(tessari_types::Number::Integer(7))
        );
        assert_eq!(
            identity_value(&RecordId::Text("a'b".to_owned())),
            Value::from("a'b"),
            "a quote in an identity must stay a quote in a value"
        );
    }
}
