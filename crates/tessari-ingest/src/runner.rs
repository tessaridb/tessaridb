//! Starting what the catalog declares, and stopping it cleanly.
//!
//! # One OS thread per consumer, beside the runtime
//!
//! The node serves from one async runtime (ADR-0085), and a consumer is the one
//! thing it runs that is not a task on it. A consumer holds a broker client
//! that is a blocking C library, and waits in it for as long as a quiet topic
//! is quiet; on the runtime's blocking pool that wait would hold one of the
//! threads the store calls are bounded to, for the life of the process. A
//! long-lived blocking client on a thread of its own is the shape a thread is
//! for, and what the runtime shares with it is only the moment it is told to
//! stop.
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

mod declared;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use tessari_session::Session;
use tessari_storage::{ConsumerDefinition, OnFailure, Store};
use tessari_types::{RecordId, Value};

use crate::apply::shape;
use crate::source::{Source, SourceError};
pub(crate) use declared::declarations;

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
    /// serving, and `INFO FOR KAFKA CONSUMER` reports it as not running here.
    ///
    /// # Errors
    ///
    /// Returns the store's failure when the catalog cannot be read at all.
    pub fn start(
        store: &Store,
        broker: Arc<dyn Broker>,
    ) -> Result<Started, tessari_storage::Error> {
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

/// Where a consumer's records go, named as a statement would name it.
#[derive(Debug, Clone)]
pub(crate) struct Destination {
    namespace: String,
    database: String,
    table: String,
}

/// Whether a name can stand unquoted in a statement.
pub(crate) fn plain(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with(|first: char| first.is_ascii_digit())
        && name
            .chars()
            .all(|held| held.is_ascii_alphanumeric() || held == '_')
}

/// Forgets a consumer when the thread that ran it ends, however it ends.
///
/// Not restarted after a panic: the message that caused it is redelivered to
/// the next attempt, so a restart would meet the same defect again.
struct Leaving<'a> {
    store: &'a Store,
    name: &'a str,
}

impl Drop for Leaving<'_> {
    fn drop(&mut self) {
        self.store.running().stopped(self.name);
    }
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
        // However this thread leaves — told to stop, halted by a failure, or
        // unwinding from a panic — the registry entry goes with it, so `INFO FOR
        // KAFKA CONSUMER` reports it as not running here rather than as running
        // and stuck. A guard rather than a call at each exit, because a panic is
        // the exit nobody writes a call for.
        let _leaving = Leaving {
            store: &self.store,
            name: &self.definition.name,
        };
        while !self.stopping.load(Ordering::Relaxed) {
            match self.once(source.as_mut()) {
                Ok(()) => {}
                Err(failure) => {
                    log::error!("consumer {} stopped: {failure}", self.definition.name);
                    // `stop` halts this consumer and leaves the rest alone.
                    self.store
                        .running()
                        .advanced(&self.definition.name, |progress| {
                            progress.last_error = Some(failure.clone());
                        });
                    return;
                }
            }
        }
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
        let mut parked: Vec<Value> = Vec::new();
        let mut parked_at: Vec<(i32, i64)> = Vec::new();
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
                        parked.push(quarantine_record(message, &why.to_string()));
                        parked_at.push((message.partition, message.offset));
                    }
                },
            }
        }

        let applied = u64::try_from(records.len()).unwrap_or(u64::MAX);
        // **The store first.** A crash between here and the offset commit
        // redelivers the batch, which re-applies to the same identities.
        self.write(&records, &parked_at, &parked)?;

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
                progress.quarantined = progress
                    .quarantined
                    .saturating_add(u64::try_from(parked.len()).unwrap_or(u64::MAX));
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
    ///
    /// The messages the batch quarantined are parked in the same transaction,
    /// so they are kept exactly when their neighbours land (Q-708).
    fn write(
        &self,
        records: &[crate::apply::Shaped],
        parked_at: &[(i32, i64)],
        parked: &[Value],
    ) -> Result<(), String> {
        if records.is_empty() && parked.is_empty() {
            return Ok(());
        }
        let mut script = format!(
            "USE NAMESPACE {}; USE DATABASE {};",
            self.table.namespace, self.table.database
        );
        let mut parameters = tessari_session::Parameters::new();
        for (at, shaped) in records.iter().enumerate() {
            script.push_str(&format!(" SET {}:$id{at} = $rec{at};", self.table.table));
            parameters.insert(format!("id{at}"), identity_value(&shaped.id));
            parameters.insert(format!("rec{at}"), shaped.record.clone());
        }

        let mut attempt = 0_u32;
        loop {
            let mut session = Session::new(&self.store);
            // The declarer's authority, re-established from the catalog on every
            // batch rather than captured once. That is what makes a revocation
            // take effect on the next batch instead of never: until this, the
            // loop wrote as nobody, so demoting the declarer, revoking their
            // authority or deleting the account outright did not stop it.
            //
            // A declaration predating the field keeps writing unbound, which is
            // deliberate — narrowing an existing consumer on upgrade would be an
            // outage delivered as a migration — and `INFO FOR KAFKA CONSUMER` reports
            // the absence so an operator can find it and redeclare.
            if let Some(declarer) = self.definition.declarer
                && let Err(refused) = session.acting_as(declarer)
            {
                // A deleted declarer. Reported and retried rather than swallowed,
                // because the batch is not written and a consumer that silently
                // dropped messages here would be worse than one that stops.
                return Err(format!(
                    "consumer {} cannot act as the user that declared it: {refused}",
                    self.definition.name
                ));
            }
            let written = session.atomically(|work| {
                if !records.is_empty() {
                    work.run_with(&script, &parameters)?;
                }
                for (at, record) in parked_at.iter().zip(parked) {
                    work.keep_quarantined(self.definition.id, *at, record)?;
                }
                Ok::<(), tessari_session::Error>(())
            });
            match written {
                Ok(()) => return Ok(()),
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

/// What a quarantined message is kept as: where it came from, why it was
/// refused, when, and the payload — as text when it is text, which a JSON
/// payload that failed to parse still is, and as bytes when it is not.
fn quarantine_record(message: &crate::Message, why: &str) -> Value {
    let payload = match std::str::from_utf8(&message.payload) {
        Ok(text) => Value::from(text),
        Err(_) => Value::Bytes(message.payload.clone()),
    };
    let at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|since| {
            tessari_types::Datetime::new(
                i64::try_from(since.as_secs()).unwrap_or(i64::MAX),
                since.subsec_nanos(),
            )
        })
        .map_or(Value::None, Value::Datetime);
    Value::Object(std::collections::BTreeMap::from([
        (
            "partition".to_owned(),
            Value::Number(tessari_types::Number::Integer(i64::from(message.partition))),
        ),
        (
            "offset".to_owned(),
            Value::Number(tessari_types::Number::Integer(message.offset)),
        ),
        ("reason".to_owned(), Value::from(why)),
        ("payload".to_owned(), payload),
        ("at".to_owned(), at),
    ]))
}

/// A record identity, as the value a parameter carries.
pub(crate) fn identity_value(id: &RecordId) -> Value {
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
mod tests;
