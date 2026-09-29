//! One batch of a topic consumer: read, shape, write, acknowledge — in one
//! transaction (ADR-0087 §2).
//!
//! This is the synchronous half, run through `spawn_blocking`. A batch is
//! bounded by its `LIMIT` and by the group's width, so it always ends, which is
//! what makes an uncancellable blocking call acceptable here.

use tessari_session::{Error, Outcome, Parameters, Session};
use tessari_storage::{ConsumerDefinition, OnFailure, Store};
use tessari_types::{Number, Value};

use super::Names;
use crate::apply::shape_value;
use crate::runner::identity_value;

/// What one batch did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Batch {
    /// Committed: this many records written and messages acknowledged, and
    /// this many handed back to the group.
    Applied { applied: u64, quarantined: u64 },
    /// The group had nothing to hand out.
    Empty,
    /// This node may not write here; nothing changed, try again later.
    NotHere,
    /// Two writers met on the group or a record; nothing changed.
    Contended,
    /// The write was refused for a reason a message may carry; nothing changed,
    /// and the caller narrows to one message to find which.
    Refused(String),
}

/// Why a member stops: its declaration cannot be carried out, or its failure
/// policy said to halt. Nothing of the batch it was in is written.
#[derive(Debug)]
pub(crate) enum Stopped {
    /// The reason, as the operator reads it in `INFO FOR TOPIC CONSUMER`.
    Halt(String),
    /// A refusal from the store, classified by [`classify`].
    Store(Error),
}

impl From<Error> for Stopped {
    fn from(failure: Error) -> Self {
        Self::Store(failure)
    }
}

/// One message as a group read hands it out.
struct Handed {
    position: i64,
    payload: Value,
}

/// Read up to `limit`, apply what can be applied, acknowledge it, and hand back
/// what the policy quarantines — or change nothing.
pub(crate) fn run_batch(
    store: &Store,
    definition: &ConsumerDefinition,
    names: &Names,
    limit: u64,
) -> Result<Batch, String> {
    let mut session = Session::new(store);
    // The declarer's authority, re-established every batch, so a revocation
    // stops the consumer at its next batch rather than never.
    if let Some(declarer) = definition.declarer {
        session
            .acting_as(declarer)
            .map_err(|refused| format!("it cannot act as the user that declared it: {refused}"))?;
    }
    let answered = session.atomically(|work| {
        let read = work.run_with(
            &format!(
                "{}READ FROM {} FOR CONSUMER '{}' LIMIT {limit};",
                names.tenancy, names.topic, names.group
            ),
            &Parameters::new(),
        )?;
        let handed = handed_out(&read)?;
        if handed.is_empty() {
            return Ok(Batch::Empty);
        }
        let mut script = names.tenancy.clone();
        let mut parameters = Parameters::new();
        let mut acknowledged = Vec::with_capacity(handed.len());
        let mut returned = Vec::new();
        for message in &handed {
            match shape_value(&message.payload, definition) {
                Ok(shaped) => {
                    let at = acknowledged.len();
                    script.push_str(&format!(" SET {}:$id{at} = $rec{at};", names.table));
                    parameters.insert(format!("id{at}"), identity_value(&shaped.id));
                    parameters.insert(format!("rec{at}"), shaped.record);
                    acknowledged.push(message.position);
                }
                Err(why) => match definition.on_failure {
                    OnFailure::Stop => {
                        return Err(Stopped::Halt(format!(
                            "message at position {} could not be applied: {why}",
                            message.position
                        )));
                    }
                    OnFailure::Quarantine => returned.push(message.position),
                },
            }
        }
        settle_list(
            &mut script,
            &mut parameters,
            "ACK",
            "a",
            &acknowledged,
            names,
        );
        settle_list(&mut script, &mut parameters, "NACK", "q", &returned, names);
        work.run_with(&script, &parameters)?;
        Ok(Batch::Applied {
            applied: u64::try_from(acknowledged.len()).unwrap_or(u64::MAX),
            quarantined: u64::try_from(returned.len()).unwrap_or(u64::MAX),
        })
    });
    match answered {
        Ok(batch) => Ok(batch),
        Err(Stopped::Halt(reason)) => Err(reason),
        Err(Stopped::Store(failure)) => classify(failure),
    }
}

/// Hand back the one message a group would hand out next, as a `quarantine`
/// consumer does with a message whose write is refused — or halt, for `stop`.
pub(crate) fn hand_back_one(
    store: &Store,
    definition: &ConsumerDefinition,
    names: &Names,
    why: &str,
) -> Result<Batch, String> {
    if definition.on_failure == OnFailure::Stop {
        return Err(format!("a message could not be written: {why}"));
    }
    let mut session = Session::new(store);
    if let Some(declarer) = definition.declarer {
        session
            .acting_as(declarer)
            .map_err(|refused| format!("it cannot act as the user that declared it: {refused}"))?;
    }
    let answered = session.atomically(|work| {
        let read = work.run_with(
            &format!(
                "{}READ FROM {} FOR CONSUMER '{}' LIMIT 1;",
                names.tenancy, names.topic, names.group
            ),
            &Parameters::new(),
        )?;
        let handed = handed_out(&read)?;
        let positions: Vec<i64> = handed.iter().map(|message| message.position).collect();
        if positions.is_empty() {
            return Ok(Batch::Empty);
        }
        let mut script = names.tenancy.clone();
        let mut parameters = Parameters::new();
        settle_list(&mut script, &mut parameters, "NACK", "q", &positions, names);
        work.run_with(&script, &parameters)?;
        Ok(Batch::Applied {
            applied: 0,
            quarantined: 1,
        })
    });
    match answered {
        Ok(batch) => Ok(batch),
        Err(Stopped::Halt(reason)) => Err(reason),
        Err(Stopped::Store(failure)) => classify(failure),
    }
}

/// `ACK`/`NACK <topic> FOR CONSUMER '<group>' AT $x0, …` for these positions,
/// appended with its parameters; nothing when there are none.
fn settle_list(
    script: &mut String,
    parameters: &mut Parameters,
    verb: &str,
    prefix: &str,
    positions: &[i64],
    names: &Names,
) {
    if positions.is_empty() {
        return;
    }
    let references: Vec<String> = positions
        .iter()
        .enumerate()
        .map(|(at, position)| {
            parameters.insert(
                format!("{prefix}{at}"),
                Value::Number(Number::Integer(*position)),
            );
            format!("${prefix}{at}")
        })
        .collect();
    script.push_str(&format!(
        " {verb} {} FOR CONSUMER '{}' AT {};",
        names.topic,
        names.group,
        references.join(", ")
    ));
}

/// The messages a group read answered, in the order handed out.
fn handed_out(read: &[Outcome]) -> Result<Vec<Handed>, Stopped> {
    let Some(Outcome::Records { records, .. }) = read.last() else {
        return Err(Stopped::Halt(
            "the group read did not answer records".to_owned(),
        ));
    };
    records
        .iter()
        .map(|(_, body)| {
            let Value::Object(fields) = body else {
                return Err(Stopped::Halt("a message is not an object".to_owned()));
            };
            let Some(Value::Number(Number::Integer(position))) = fields.get("position") else {
                return Err(Stopped::Halt("a message carries no position".to_owned()));
            };
            Ok(Handed {
                position: *position,
                payload: fields.get("value").cloned().unwrap_or(Value::None),
            })
        })
        .collect()
}

/// What a refused batch means for the member that ran it.
///
/// A node that may not write is told to wait; two writers meeting is retried;
/// a refusal naming the consumer's own configuration — its declarer, its
/// group, its topic or table gone — halts it, because every message would meet
/// it; anything else may be one message's doing.
fn classify(failure: Error) -> Result<Batch, String> {
    match failure {
        Error::NotWritable { .. } | Error::Store(tessari_storage::Error::LeaseSpent { .. }) => {
            Ok(Batch::NotHere)
        }
        Error::Store(
            tessari_storage::Error::Conflict { .. }
            | tessari_storage::Error::CommitContention { .. },
        ) => Ok(Batch::Contended),
        Error::Unknown { .. }
        | Error::NotATopic { .. }
        | Error::NoSuchGroup { .. }
        | Error::NotAGroup { .. }
        | Error::NotSignedIn { .. }
        | Error::RoleForbids { .. }
        | Error::NotGranted { .. }
        | Error::OutsideTenancy { .. } => Err(failure.to_string()),
        other => Ok(Batch::Refused(other.to_string())),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

    use std::sync::Arc;

    use tessari_kv::{KvBackend, MemoryBackend};
    use tessari_session::{Outcome, Session};
    use tessari_storage::Store;
    use tessari_types::Value;

    use super::{Batch, hand_back_one, run_batch};
    use crate::topic::{Names, declared};

    /// A store with `shop.live.orders`, its dead letter, a group that parks a
    /// message after three deliveries, a strict destination, and a consumer
    /// declared over them with `policy`.
    fn shaped(policy: &str) -> Store {
        let store = Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap();
        Session::new(&store)
            .run(&format!(
                "DEFINE NAMESPACE shop; USE NAMESPACE shop; DEFINE DATABASE live; \
                 USE DATABASE live; DEFINE TOPIC orders; DEFINE TOPIC orders_dead; \
                 DEFINE TABLE order_rows (total int); \
                 DEFINE GROUP 'rows' ON TOPIC orders ACK DEADLINE 30s IN FLIGHT 100 \
                 DELIVERIES 3 DEAD LETTER TO orders_dead; \
                 DEFINE TOPIC CONSUMER orders_in FROM orders GROUP 'rows' INTO order_rows \
                 IDENTITY order_id MAP amount AS total ON FAILURE {policy};"
            ))
            .unwrap();
        store
    }

    fn publish(store: &Store, messages: &str) {
        Session::new(store)
            .run(&format!(
                "USE NAMESPACE shop; USE DATABASE live; {messages}"
            ))
            .unwrap();
    }

    fn the_consumer(store: &Store) -> (tessari_storage::ConsumerDefinition, Names) {
        let mut found = declared(store).unwrap();
        assert_eq!(found.len(), 1, "one topic consumer is declared");
        found.remove(0)
    }

    fn rows(store: &Store) -> Vec<(String, Value)> {
        let answered = Session::new(store)
            .run("USE NAMESPACE shop; USE DATABASE live; SELECT * FROM order_rows;")
            .unwrap();
        let Some(Outcome::Records { records, .. }) = answered.last() else {
            panic!("not records: {answered:?}");
        };
        records
            .iter()
            .map(|(id, value)| (id.to_string(), value.clone()))
            .collect()
    }

    /// One field of the group's report in `INFO FOR TOPIC`, as text.
    fn group_field(store: &Store, field: &str) -> String {
        let answered = Session::new(store)
            .run("USE NAMESPACE shop; USE DATABASE live; INFO FOR TOPIC orders;")
            .unwrap();
        let Some(Outcome::Value(Value::Object(report))) = answered.last() else {
            panic!("not a report: {answered:?}");
        };
        let Some(Value::Object(groups)) = report.get("groups") else {
            panic!("no groups: {report:?}");
        };
        let Some(Value::Object(group)) = groups.get("rows") else {
            panic!("no group rows: {groups:?}");
        };
        group
            .get(field)
            .map(ToString::to_string)
            .unwrap_or_default()
    }

    #[test]
    fn every_message_lands_once_with_only_the_mapped_fields_and_nothing_is_left_in_flight() {
        let store = shaped("quarantine");
        publish(
            &store,
            "CREATE orders:'a' = { order_id: 1, amount: 10, extra: true }; \
             CREATE orders:'b' = { order_id: 2, amount: 20 }; \
             CREATE orders:'c' = { order_id: 'x', amount: 30 };",
        );
        let (definition, names) = the_consumer(&store);
        assert_eq!(
            run_batch(&store, &definition, &names, 500),
            Ok(Batch::Applied {
                applied: 3,
                quarantined: 0
            })
        );
        let landed = rows(&store);
        assert_eq!(landed.len(), 3, "{landed:?}");
        assert!(
            landed.iter().all(|(_, record)| {
                matches!(record, Value::Object(fields) if fields.len() == 1 && fields.contains_key("total"))
            }),
            "a field nobody mapped landed: {landed:?}"
        );
        assert_eq!(group_field(&store, "in_flight"), "0");
        assert_eq!(group_field(&store, "committed"), "3");
        // Read again: the group hands out nothing, so nothing lands twice.
        assert_eq!(
            run_batch(&store, &definition, &names, 500),
            Ok(Batch::Empty)
        );
        assert_eq!(rows(&store).len(), 3);
    }

    #[test]
    fn a_refused_write_changes_nothing_not_even_the_group() {
        let store = shaped("quarantine");
        publish(
            &store,
            "CREATE orders:'a' = { order_id: 1, amount: 10 }; \
             CREATE orders:'b' = { order_id: 2, amount: 'not a number' }; \
             CREATE orders:'c' = { order_id: 3, amount: 30 };",
        );
        let (definition, names) = the_consumer(&store);
        let refused = run_batch(&store, &definition, &names, 500);
        assert!(matches!(refused, Ok(Batch::Refused(_))), "{refused:?}");
        assert!(rows(&store).is_empty(), "a refused batch wrote something");
        assert_eq!(group_field(&store, "position"), "0", "the read was kept");
        assert_eq!(group_field(&store, "in_flight"), "0");

        // One at a time: the first lands, the second is the one refused.
        assert_eq!(
            run_batch(&store, &definition, &names, 1),
            Ok(Batch::Applied {
                applied: 1,
                quarantined: 0
            })
        );
        let alone = run_batch(&store, &definition, &names, 1);
        let Ok(Batch::Refused(why)) = alone else {
            panic!("the bad message was not refused alone: {alone:?}");
        };
        assert_eq!(
            hand_back_one(&store, &definition, &names, &why),
            Ok(Batch::Applied {
                applied: 0,
                quarantined: 1
            })
        );
        assert_eq!(group_field(&store, "redelivered"), "0");
        assert_eq!(rows(&store).len(), 1);
    }

    #[test]
    fn quarantine_hands_a_message_back_until_the_group_dead_letters_it_and_the_rest_flow() {
        let store = shaped("quarantine");
        publish(
            &store,
            "CREATE orders:'a' = { amount: 10 }; \
             CREATE orders:'b' = { order_id: 2, amount: 20 };",
        );
        let (definition, names) = the_consumer(&store);
        for _ in 0..6 {
            if run_batch(&store, &definition, &names, 500) == Ok(Batch::Empty) {
                break;
            }
        }
        assert_eq!(rows(&store).len(), 1, "the good message is not held up");
        assert_eq!(group_field(&store, "dead_lettered"), "1");
        assert_eq!(group_field(&store, "in_flight"), "0");
    }

    #[test]
    fn stop_halts_at_a_message_it_cannot_apply_and_writes_nothing_of_its_batch() {
        let store = shaped("stop");
        publish(
            &store,
            "CREATE orders:'a' = { order_id: 1, amount: 10 }; \
             CREATE orders:'b' = { amount: 20 };",
        );
        let (definition, names) = the_consumer(&store);
        let halted = run_batch(&store, &definition, &names, 500);
        let Err(reason) = halted else {
            panic!("stop did not halt: {halted:?}");
        };
        assert!(reason.contains("position 2"), "{reason}");
        assert!(rows(&store).is_empty(), "the batch was partly written");
        assert_eq!(group_field(&store, "in_flight"), "0");
        assert!(hand_back_one(&store, &definition, &names, "why").is_err());
    }

    #[test]
    fn a_node_that_may_not_write_waits_and_a_lost_declarer_halts() {
        let store = shaped("quarantine");
        publish(&store, "CREATE orders:'a' = { order_id: 1, amount: 10 };");
        let (mut definition, names) = the_consumer(&store);

        Session::new(&store)
            .run("DEFINE NODE ROLES serving;")
            .unwrap();
        assert_eq!(
            run_batch(&store, &definition, &names, 500),
            Ok(Batch::NotHere)
        );
        assert!(rows(&store).is_empty());
        Session::new(&store)
            .run("DEFINE NODE ROLES serving, writable;")
            .unwrap();

        definition.declarer = Some(9_999);
        let halted = run_batch(&store, &definition, &names, 500);
        assert!(
            matches!(&halted, Err(reason) if reason.contains("declared it")),
            "{halted:?}"
        );
        assert!(rows(&store).is_empty());
    }
}
