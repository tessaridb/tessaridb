//! How a declared stream consumer is described, and what it has done.

use std::collections::BTreeMap;
use tessari_storage::{ConsumerDefinition, Progress};
use tessari_types::Value;

/// What a consumer promises, and what it refuses to.
///
/// Built per answer rather than held in a constant, because a [`Value`] cannot
/// be one — and the cost is irrelevant: this runs once per administrative
/// statement, not once per record.
///
/// It is part of the report rather than of the documentation alone because the
/// failure being avoided is a documented one: the system that has shipped this
/// feature longest states its delivery guarantee in a guide and a design
/// proposal, and *not* on the page somebody reads while configuring a consumer.
/// The reader of this output is configuring one right now.
pub(crate) fn guarantees() -> Value {
    Value::Object(BTreeMap::from([
        ("delivery".to_owned(), Value::from("at-least-once")),
        (
            "idempotence".to_owned(),
            Value::from(
                "a replayed message converges to one record, because the identity field \
                 makes the write a compare-and-set",
            ),
        ),
        (
            "exactly_once".to_owned(),
            Value::from(
                "not offered: the store commit and the broker offset commit are two \
                 commits into two systems, and the store's comes first, which chooses \
                 duplicates over loss",
            ),
        ),
        (
            "schema".to_owned(),
            Value::from("declared, never inferred: a message field nobody mapped does not land"),
        ),
    ]))
}

/// One Kafka consumer's declaration, as an object; `source` is its brokers,
/// topic and format, taken out of its feed by the caller.
pub(crate) fn described_consumer(
    consumer: &ConsumerDefinition,
    (brokers, topic, format): (&[String], &str, &str),
    destination: &str,
) -> Value {
    let brokers = brokers
        .iter()
        .map(|broker| Value::from(broker.as_str()))
        .collect();
    let Value::Object(mut described) = described_common(consumer, destination) else {
        return Value::None;
    };
    described.insert("brokers".to_owned(), Value::Array(brokers));
    described.insert("topic".to_owned(), Value::from(topic));
    described.insert("format".to_owned(), Value::from(format));
    Value::Object(described)
}

/// What both kinds of consumer declare: its name, group, mapping, destination,
/// failure policy, parallelism and declarer.
pub(crate) fn described_common(consumer: &ConsumerDefinition, destination: &str) -> Value {
    let mapping = consumer
        .mapping
        .iter()
        .map(|pair| {
            Value::Object(BTreeMap::from([
                ("from".to_owned(), Value::from(pair.from.as_str())),
                ("to".to_owned(), Value::from(pair.to.as_str())),
            ]))
        })
        .collect();
    Value::Object(BTreeMap::from([
        ("name".to_owned(), Value::from(consumer.name.as_str())),
        ("group".to_owned(), Value::from(consumer.group.as_str())),
        (
            "identity".to_owned(),
            Value::from(consumer.identity.as_str()),
        ),
        ("mapping".to_owned(), Value::Array(mapping)),
        ("destination".to_owned(), Value::from(destination)),
        (
            "on_failure".to_owned(),
            Value::from(consumer.on_failure.spelling()),
        ),
        (
            "parallelism".to_owned(),
            Value::Number(tessari_types::Number::Integer(i64::from(
                consumer.parallelism,
            ))),
        ),
        // Whose authority its writes carry. Reported as a **word** and not as
        // an absent field when there is none, because the absence is the one
        // an operator has to act on: a consumer declared before this existed
        // writes unbound, and a field that simply vanished would leave no way
        // to find which ones. `NULL` here would read as *no information*; this
        // reads as *nobody*, which is what it is.
        (
            "declarer".to_owned(),
            consumer.declarer.map_or_else(
                || Value::from("unbound — declared before writes carried an identity"),
                |id| Value::Number(tessari_types::Number::Integer(i64::from(id))),
            ),
        ),
    ]))
}

/// What this process is doing, or that it is doing nothing.
pub(crate) fn running_state(progress: Option<&Progress>) -> Value {
    let Some(progress) = progress else {
        // Named rather than left as an absent field, because "this node is not
        // running it" is the answer an operator is most often looking for, and
        // an empty object would read as "no information".
        return Value::Object(BTreeMap::from([("here".to_owned(), Value::Bool(false))]));
    };
    let positions = progress
        .positions
        .iter()
        .map(|(partition, offset)| {
            Value::Object(BTreeMap::from([
                (
                    "partition".to_owned(),
                    Value::Number(tessari_types::Number::Integer(i64::from(*partition))),
                ),
                (
                    "offset".to_owned(),
                    Value::Number(tessari_types::Number::Integer(*offset)),
                ),
            ]))
        })
        .collect();
    Value::Object(BTreeMap::from([
        ("here".to_owned(), Value::Bool(true)),
        ("halted".to_owned(), Value::Bool(progress.halted)),
        (
            "applied".to_owned(),
            Value::Number(tessari_types::Number::Integer(
                i64::try_from(progress.applied).unwrap_or(i64::MAX),
            )),
        ),
        (
            "quarantined".to_owned(),
            Value::Number(tessari_types::Number::Integer(
                i64::try_from(progress.quarantined).unwrap_or(i64::MAX),
            )),
        ),
        (
            "last_error".to_owned(),
            progress
                .last_error
                .as_deref()
                .map_or(Value::Null, Value::from),
        ),
        ("positions".to_owned(), Value::Array(positions)),
    ]))
}
