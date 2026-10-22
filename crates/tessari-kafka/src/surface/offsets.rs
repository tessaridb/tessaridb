//! A topic's offsets as the protocol counts them: one below its positions.
//!
//! Positions start at 1 and are dense; Kafka offsets start at 0. The message at
//! position `p` is offset `p − 1`, the log start is the first position held
//! minus one, and the high watermark — the offset the next message takes — is
//! the last position given.

use std::sync::Arc;

use tessaridb::{Outcome, Parameters, Value};

use crate::api::list_offsets::{self, Answer, Topic as Asked};
use crate::surface::Shared;
use crate::surface::errors::{self, code};
use crate::surface::names::{self, Topic};
use crate::surface::running::{Failed, Held};

/// A topic's first offset held and the offset its next message will take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Bounds {
    pub(crate) log_start: i64,
    pub(crate) high_watermark: i64,
}

/// The bounds `INFO FOR TOPIC` answered.
pub(crate) fn bounds_of(info: &Value) -> Bounds {
    let field = |name: &str| match info {
        Value::Object(fields) => match fields.get(name) {
            Some(Value::Number(tessaridb::Number::Integer(held))) => Some(*held),
            _ => None,
        },
        _ => None,
    };
    let high_watermark = field("last").unwrap_or(0).max(0);
    let log_start = field("first").map_or(high_watermark, |first| first.saturating_sub(1));
    Bounds {
        log_start,
        high_watermark,
    }
}

/// What `INFO FOR TOPIC` reports about one partition (ADR-0131): a split
/// topic's own entry for it, or the whole report of a topic never split, which
/// is its own partition 0; `None` for a partition the topic does not have.
pub(crate) fn partition_info(info: &Value, index: i32) -> Option<&Value> {
    let Value::Object(fields) = info else {
        return None;
    };
    match fields.get("each") {
        Some(Value::Array(each)) => usize::try_from(index).ok().and_then(|at| each.get(at)),
        _ => (index == 0).then_some(info),
    }
}

/// How many partitions `INFO FOR TOPIC` reports: one for a topic never split.
pub(crate) fn partitions_in(info: &Value) -> usize {
    match info {
        Value::Object(fields) => match fields.get("each") {
            Some(Value::Array(each)) => each.len(),
            _ => 1,
        },
        _ => 1,
    }
}

/// The parameters naming one partition, for a statement that says
/// `PARTITION $partition`.
pub(crate) fn partition_parameter(index: i32) -> (String, Value) {
    (
        "partition".to_owned(),
        Value::Number(tessaridb::Number::Integer(i64::from(index))),
    )
}

/// One partition's bounds, read through the caller's session; `None` when the
/// topic has no such partition.
pub(crate) async fn bounds(
    shared: &Arc<Shared>,
    held: &mut Held,
    topic: &Topic<'_>,
    index: i32,
) -> Result<Option<Bounds>, Failed> {
    let script = format!("INFO FOR TOPIC {};", topic.name);
    let outcomes = held
        .run(shared, topic.tenancy(), &script, &Parameters::new())
        .await?;
    Ok(match outcomes.last() {
        Some(Outcome::Value(info)) => partition_info(info, index).map(bounds_of),
        _ => None,
    })
}

/// A datetime as whole milliseconds since the epoch, when it is one.
pub(crate) fn millis_of(value: Option<&Value>) -> Option<i64> {
    match value {
        Some(Value::Datetime(at)) => at
            .seconds()
            .checked_mul(1000)
            .and_then(|millis| millis.checked_add(i64::from(at.nanos() / 1_000_000))),
        _ => None,
    }
}

/// A message's timestamp in milliseconds: the one a Kafka client wrote, or
/// else the instant the message was appended (ADR-0133 D3).
fn timestamp_of(message: &Value) -> Option<i64> {
    let Value::Object(fields) = message else {
        return None;
    };
    let Some(Value::Object(inner)) = fields.get("value") else {
        return millis_of(fields.get("appended"));
    };
    millis_of(inner.get("timestamp")).or_else(|| millis_of(fields.get("appended")))
}

/// The first offset at or after `target` milliseconds, by halving the range:
/// positions are dense, and a producer's timestamps rise with them.
async fn at_time(
    shared: &Arc<Shared>,
    held: &mut Held,
    (topic, index): (&Topic<'_>, i32),
    bounds: Bounds,
    target: i64,
) -> Result<(i64, i64), Failed> {
    let (mut low, mut high) = (bounds.log_start, bounds.high_watermark);
    let mut found = (-1, -1);
    let script = format!(
        "READ FROM {} PARTITION $partition AFTER $after LIMIT 1;",
        topic.name
    );
    while low < high {
        let middle = low.saturating_add(high.saturating_sub(low) / 2);
        let parameters = Parameters::from([
            (
                "after".to_owned(),
                Value::Number(tessaridb::Number::Integer(middle)),
            ),
            partition_parameter(index),
        ]);
        let outcomes = held
            .run(shared, topic.tenancy(), &script, &parameters)
            .await?;
        let stamp = match outcomes.last() {
            Some(Outcome::Records { records, .. }) => records
                .first()
                .and_then(|(_, message)| timestamp_of(message)),
            _ => None,
        };
        match stamp {
            Some(stamp) if stamp >= target => {
                found = (stamp, middle);
                high = middle;
            }
            _ => low = middle.saturating_add(1),
        }
    }
    Ok(found)
}

/// Answer a ListOffsets request.
pub(crate) async fn list(
    shared: &Arc<Shared>,
    held: &mut Held,
    topics: &[Asked<'_>],
) -> Vec<(String, Vec<Answer>)> {
    let mut answers = Vec::with_capacity(topics.len());
    for asked in topics {
        let topic = names::topic(asked.name, shared.database.as_ref());
        let mut partitions = Vec::with_capacity(asked.partitions.len());
        for partition in &asked.partitions {
            let mut answer = Answer {
                index: partition.index,
                error_code: errors::NONE,
                timestamp: -1,
                offset: -1,
            };
            let Some(topic) = topic.as_ref() else {
                answer.error_code = errors::UNKNOWN_TOPIC_OR_PARTITION;
                partitions.push(answer);
                continue;
            };
            match bounds(shared, held, topic, partition.index).await {
                Err(failed) => answer.error_code = code(&failed),
                Ok(None) => answer.error_code = errors::UNKNOWN_TOPIC_OR_PARTITION,
                Ok(Some(bounds)) => match partition.timestamp {
                    list_offsets::EARLIEST => answer.offset = bounds.log_start,
                    list_offsets::LATEST => answer.offset = bounds.high_watermark,
                    list_offsets::MAX_TIMESTAMP => {
                        answer.offset = bounds
                            .high_watermark
                            .saturating_sub(1)
                            .max(bounds.log_start);
                    }
                    target => match at_time(shared, held, (topic, partition.index), bounds, target)
                        .await
                    {
                        Ok((timestamp, offset)) => {
                            answer.timestamp = timestamp;
                            answer.offset = offset;
                        }
                        Err(failed) => answer.error_code = code(&failed),
                    },
                },
            }
            partitions.push(answer);
        }
        answers.push((asked.name.to_owned(), partitions));
    }
    answers
}
