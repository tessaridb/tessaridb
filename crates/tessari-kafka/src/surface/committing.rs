//! A group's committed offsets: the topic's named reader position
//! `kafka:<group>` (ADR-0129 D6).
//!
//! The next offset a group will read is the position it has read up to: the
//! message at position `p` is offset `p − 1`, so having read through position
//! `p` the group next reads offset `p`. A commit moves the position with
//! `READ FROM … FOR CONSUMER … AFTER … LIMIT 0`, which reads nothing and
//! moves it; a fetch reads it back from `INFO FOR TOPIC`. Both run through the
//! caller's session, so the store's grants decide who may commit.

use std::sync::Arc;

use tessaridb::{Number, Outcome, Parameters, Value};

use crate::api::committed::{CommitRequest, Committed, FetchRequest};
use crate::surface::errors::{self, code};
use crate::surface::names;
use crate::surface::running::Held;
use crate::surface::{Shared, listing};

/// The reader position a group's offsets are kept under.
fn reader(group: &str) -> String {
    format!("kafka:{group}")
}

/// Commit each partition's offset; a code per partition.
pub(crate) async fn commit(
    shared: &Arc<Shared>,
    held: &mut Held,
    request: &CommitRequest<'_>,
) -> Vec<(String, Vec<(i32, i16)>)> {
    let allowed = shared
        .coordinator
        .may_commit(
            held.user(),
            request.group,
            request.generation,
            request.member,
        )
        .await;
    let name = reader(request.group);
    let mut answers = Vec::with_capacity(request.topics.len());
    for (asked, partitions) in &request.topics {
        let topic = names::topic(asked, shared.database.as_ref());
        let mut codes = Vec::with_capacity(partitions.len());
        for partition in partitions {
            let answered = match &topic {
                _ if allowed != errors::NONE => allowed,
                None => errors::UNKNOWN_TOPIC_OR_PARTITION,
                Some(_) if partition.index != 0 => errors::UNKNOWN_TOPIC_OR_PARTITION,
                Some(_) if partition.offset < 0 => errors::OFFSET_OUT_OF_RANGE,
                Some(topic) => {
                    let script = format!(
                        "READ FROM {} FOR CONSUMER $name AFTER $offset LIMIT 0;",
                        topic.name
                    );
                    let parameters = Parameters::from([
                        ("name".to_owned(), Value::String(name.clone())),
                        (
                            "offset".to_owned(),
                            Value::Number(Number::Integer(partition.offset)),
                        ),
                    ]);
                    match held
                        .run(shared, topic.tenancy(), &script, &parameters)
                        .await
                    {
                        Ok(_) => errors::NONE,
                        Err(failed) => code(&failed),
                    }
                }
            };
            codes.push((partition.index, answered));
        }
        answers.push(((*asked).to_owned(), codes));
    }
    answers
}

/// The position a topic's reader `name` stands at, `None` when it never read.
fn position_of(info: &Value, name: &str) -> Option<i64> {
    let Value::Object(fields) = info else {
        return None;
    };
    let Some(Value::Object(consumers)) = fields.get("consumers") else {
        return None;
    };
    let Some(Value::Object(consumer)) = consumers.get(name) else {
        return None;
    };
    match consumer.get("position") {
        Some(Value::Number(Number::Integer(position))) => Some(*position),
        _ => None,
    }
}

/// Fetch committed offsets: the topics asked about, or every topic the caller
/// may see that the group has read.
pub(crate) async fn fetch(
    shared: &Arc<Shared>,
    held: &mut Held,
    request: &FetchRequest<'_>,
) -> Vec<(String, Vec<Committed>)> {
    let name = reader(request.group);
    let every = request.topics.is_none();
    let asked: Vec<(String, Vec<i32>)> = match &request.topics {
        Some(topics) => topics
            .iter()
            .map(|(topic, partitions)| ((*topic).to_owned(), partitions.clone()))
            .collect(),
        None => listing::visible(shared, held)
            .await
            .into_iter()
            .map(|topic| (topic, vec![0]))
            .collect(),
    };
    let mut answers = Vec::with_capacity(asked.len());
    for (asked, partitions) in asked {
        let (offset, found) = match names::topic(&asked, shared.database.as_ref()) {
            None => (-1, errors::UNKNOWN_TOPIC_OR_PARTITION),
            Some(topic) => {
                let script = format!("INFO FOR TOPIC {};", topic.name);
                match held
                    .run(shared, topic.tenancy(), &script, &Parameters::new())
                    .await
                {
                    Ok(outcomes) => match outcomes.last() {
                        Some(Outcome::Value(info)) => {
                            (position_of(info, &name).unwrap_or(-1), errors::NONE)
                        }
                        _ => (-1, errors::NONE),
                    },
                    Err(failed) => (-1, code(&failed)),
                }
            }
        };
        if every && (offset < 0 || found != errors::NONE) {
            continue;
        }
        let committed = partitions
            .iter()
            .map(|index| match *index {
                0 => Committed {
                    index: 0,
                    offset,
                    error_code: found,
                },
                other => Committed {
                    index: other,
                    offset: -1,
                    error_code: errors::UNKNOWN_TOPIC_OR_PARTITION,
                },
            })
            .collect();
        answers.push((asked, committed));
    }
    answers
}
