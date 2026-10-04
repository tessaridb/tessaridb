//! Running the declared topic consumers (ADR-0087).
//!
//! # Tasks, not threads
//!
//! A Kafka consumer is an OS thread because its client is a blocking C library.
//! A topic consumer has no such client: each member is a task on the node's
//! runtime, and each batch — which is a store transaction and therefore
//! synchronous — crosses through `spawn_blocking`, as every other store call on
//! the serving edge does (ADR-0085). A batch is bounded, so the crossing ends.
//!
//! # Reconciled, not only started
//!
//! A supervisor reads the declarations once a second, starts members for a new
//! one and stops those whose declaration was dropped or replaced, so `DEFINE`
//! and `DROP TOPIC CONSUMER` take effect without a restart. A member that halts
//! on its failure policy is not restarted until its declaration changes: the
//! message that halted it would halt it again.
//!
//! # Where it may write
//!
//! Every node that holds the declaration runs it. A node that may not write the
//! destination is refused before anything changes, and its members wait and try
//! again — so the node that may write makes progress, and a failover moves the
//! consumption with the leadership.

mod batch;
#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tessari_storage::{Catalog, ConsumerDefinition, Feed, Store};
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::runner::plain;
use batch::{Batch, hand_back_one, run_batch};

/// How often the supervisor reads the declarations.
const RECONCILE: Duration = Duration::from_secs(1);
/// The first wait after a read that found nothing (consumer contract §4.5).
const FIRST_WAIT: Duration = Duration::from_millis(50);
/// The longest wait between reads that find nothing, and the wait of a node
/// that may not write.
const LONGEST_WAIT: Duration = Duration::from_secs(1);
/// The most messages one batch asks for; the group's width bounds it further.
const BATCH: u64 = 500;
/// How long a stopping member may take to finish the batch it is in. A batch
/// is bounded, so this is a ceiling on a slow store rather than a wait anybody
/// expects to reach.
const DRAIN: Duration = Duration::from_secs(30);

/// What a batch composes its statements from: names read back out of the
/// catalog and checked, never text from a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Names {
    /// `USE NAMESPACE …; USE DATABASE …; `
    pub(crate) tenancy: String,
    /// The topic read.
    pub(crate) topic: String,
    /// The table written.
    pub(crate) table: String,
    /// The group read as, written inside `'…'`.
    pub(crate) group: String,
}

/// Whether a group name can stand inside `'…'` — the pattern a group is
/// declared under, checked here because it is written into a statement.
fn plain_group(name: &str) -> bool {
    (1..=128).contains(&name.len())
        && name
            .chars()
            .all(|held| held.is_ascii_alphanumeric() || "_.:-".contains(held))
}

/// Every topic consumer declared, with the names its batches are written in.
/// A declaration whose topic or table is gone, or whose names cannot stand in
/// a statement, is left out and logged.
fn declared(store: &Store) -> Result<Vec<(ConsumerDefinition, Names)>, tessari_storage::Error> {
    let mut transaction = store.begin()?;
    let catalog = Catalog::new(&mut transaction);
    let mut found = Vec::new();
    for definition in catalog.consumers()? {
        let Feed::Topic { table: topic } = definition.feed else {
            continue;
        };
        let namespace = catalog
            .namespaces()?
            .into_iter()
            .find(|held| held.id == definition.namespace)
            .map(|held| held.name);
        let database = catalog
            .databases_in(definition.namespace)?
            .into_iter()
            .find(|held| held.id == definition.database)
            .map(|held| held.name);
        let tables = catalog.tables_in(definition.namespace, definition.database)?;
        let named = |id| {
            tables
                .iter()
                .find(|held| held.id == id)
                .map(|held| held.name.clone())
        };
        let names = match (
            namespace,
            database,
            named(topic),
            named(definition.destination),
        ) {
            (Some(namespace), Some(database), Some(topic), Some(table))
                if [&namespace, &database, &topic, &table]
                    .iter()
                    .all(|name| plain(name))
                    && plain_group(&definition.group) =>
            {
                Names {
                    tenancy: format!("USE NAMESPACE {namespace}; USE DATABASE {database}; "),
                    topic,
                    table,
                    group: definition.group.clone(),
                }
            }
            _ => {
                tracing::warn!(
                    consumer = %definition.name,
                    "a topic consumer names a topic or table that is gone, so it is not started"
                );
                continue;
            }
        };
        found.push((definition, names));
    }
    transaction.rollback();
    Ok(found)
}

/// One declaration's members on this node. Owned by the supervisor task alone,
/// so the map holding these is an ordinary one.
struct Members {
    definition: ConsumerDefinition,
    names: Names,
    signal: watch::Sender<bool>,
    tasks: Vec<JoinHandle<()>>,
}

/// Run every declared topic consumer until `stop` turns true, then let each
/// member finish the batch it is in and return once all of them have.
///
/// A future, not a runtime: the node spawns it on its own runtime, and the
/// batches reach the store through that runtime's blocking pool.
pub async fn run_topic_consumers(store: Store, mut stop: watch::Receiver<bool>) {
    let mut running: HashMap<String, Members> = HashMap::new();
    let mut ticker = tokio::time::interval(RECONCILE);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            biased;
            changed = stop.changed() => {
                if changed.is_err() || *stop.borrow() {
                    break;
                }
            }
            _ = ticker.tick() => reconcile(&store, &mut running).await,
        }
    }
    for (name, members) in running.drain() {
        leave(&store, &name, members).await;
    }
}

/// Bring what runs here into line with what is declared.
async fn reconcile(store: &Store, running: &mut HashMap<String, Members>) {
    let reading = store.clone();
    let declared = match tokio::task::spawn_blocking(move || declared(&reading)).await {
        Ok(Ok(declared)) => declared,
        Ok(Err(failure)) => {
            tracing::warn!(error = %failure, "the topic consumers could not be read");
            return;
        }
        Err(failure) => {
            tracing::warn!(error = %failure, "reading the topic consumers ended abnormally");
            return;
        }
    };
    let gone: Vec<String> = running
        .iter()
        .filter(|(name, members)| {
            !declared.iter().any(|(definition, names)| {
                &definition.name == *name
                    && *definition == members.definition
                    && *names == members.names
            })
        })
        .map(|(name, _)| name.clone())
        .collect();
    for name in gone {
        if let Some(members) = running.remove(&name) {
            leave(store, &name, members).await;
        }
    }
    for (definition, names) in declared {
        if running.contains_key(&definition.name) {
            continue;
        }
        store.running().started(&definition.name);
        let (signal, stopped) = watch::channel(false);
        let shared = (Arc::new(definition.clone()), Arc::new(names.clone()));
        let tasks = (0..definition.parallelism)
            .map(|_| {
                tokio::spawn(member(
                    store.clone(),
                    Arc::clone(&shared.0),
                    Arc::clone(&shared.1),
                    stopped.clone(),
                ))
            })
            .collect();
        running.insert(
            definition.name.clone(),
            Members {
                definition,
                names,
                signal,
                tasks,
            },
        );
    }
}

/// Stop a declaration's members, wait for the batch each is in, and forget it.
async fn leave(store: &Store, name: &str, members: Members) {
    // `send_replace` rather than `send`: it sets the value even when every
    // member has already returned, which is the state this is asking for.
    members.signal.send_replace(true);
    for task in members.tasks {
        match tokio::time::timeout(DRAIN, task).await {
            Ok(Ok(())) => {}
            Ok(Err(failure)) => {
                tracing::warn!(consumer = %name, error = %failure, "a member of a topic consumer ended abnormally");
            }
            Err(_) => {
                tracing::warn!(consumer = %name, "a member of a topic consumer did not finish its batch")
            }
        }
    }
    store.running().stopped(name);
}

/// Wait `patience`, or less when told to stop; answers whether to stop.
async fn rest(stop: &mut watch::Receiver<bool>, patience: Duration) -> bool {
    tokio::select! {
        biased;
        changed = stop.changed() => changed.is_err() || *stop.borrow(),
        () = tokio::time::sleep(patience) => *stop.borrow(),
    }
}

/// One member: batches until told to stop or halted by its failure policy.
async fn member(
    store: Store,
    definition: Arc<ConsumerDefinition>,
    names: Arc<Names>,
    mut stop: watch::Receiver<bool>,
) {
    let name = definition.name.clone();
    let mut wait = FIRST_WAIT;
    let mut limit = BATCH;
    let mut refused: Option<String> = None;
    while !*stop.borrow() {
        let (held, declared, named) = (store.clone(), Arc::clone(&definition), Arc::clone(&names));
        let narrowed = refused.clone();
        let outcome = tokio::task::spawn_blocking(move || match narrowed {
            // One message's write was refused on its own: the policy decides it.
            Some(why) => hand_back_one(&held, &declared, &named, &why),
            None => run_batch(&held, &declared, &named, limit),
        })
        .await
        .unwrap_or_else(|failure| Err(format!("a batch ended abnormally: {failure}")));
        match outcome {
            Ok(Batch::Applied {
                applied,
                quarantined,
            }) => {
                store.running().advanced(&name, |progress| {
                    progress.applied = progress.applied.saturating_add(applied);
                    progress.quarantined = progress.quarantined.saturating_add(quarantined);
                    if let Some(why) = &refused {
                        progress.last_error = Some(why.clone());
                    }
                });
                wait = FIRST_WAIT;
                limit = BATCH;
                refused = None;
            }
            Ok(Batch::Empty) => {
                limit = BATCH;
                refused = None;
                if rest(&mut stop, wait).await {
                    return;
                }
                wait = wait.saturating_mul(2).min(LONGEST_WAIT);
            }
            Ok(Batch::NotHere) => {
                if rest(&mut stop, LONGEST_WAIT).await {
                    return;
                }
            }
            Ok(Batch::Contended) => {
                if rest(&mut stop, FIRST_WAIT).await {
                    return;
                }
            }
            // Narrow to one message: the next pass either writes it — so the
            // refusal belonged to another message of the batch — or meets the
            // refusal alone, and then the failure policy decides that message.
            // A hand-back that is itself refused has nothing narrower to try.
            Ok(Batch::Refused(why)) if refused.is_some() => {
                halt(
                    &store,
                    &name,
                    format!("a message could not be handed back: {why}"),
                );
                return;
            }
            Ok(Batch::Refused(why)) => {
                if limit == 1 {
                    refused = Some(why);
                } else {
                    limit = 1;
                }
            }
            Err(reason) => {
                halt(&store, &name, reason);
                return;
            }
        }
    }
}

/// Record why a member stopped itself and keep it readable (ADR-0087 §3).
fn halt(store: &Store, name: &str, reason: String) {
    tracing::error!(consumer = %name, reason = %reason, "a topic consumer halted");
    store.running().advanced(name, |progress| {
        progress.last_error = Some(reason);
        progress.halted = true;
    });
}
