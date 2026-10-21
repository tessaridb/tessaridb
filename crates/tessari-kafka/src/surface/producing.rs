//! Produce: each batch is one `INSERT … RETURN POSITION` into the topic.

use std::sync::Arc;

use tessaridb::{Datetime, Outcome, Parameters, Value};

use crate::api::produce::{PartitionResponse, Request, TopicResponse};
use crate::records::{Batch, decode_batches};
use crate::surface::Shared;
use crate::surface::errors::{self, code};
use crate::surface::names::{self, Topic};
use crate::surface::producers::Sequenced;
use crate::surface::running::{Failed, Held};

/// The protocol's code for a batch whose checksum does not match.
const CORRUPT_MESSAGE: i16 = 2;
/// The protocol's code for a batch this node cannot take as written.
const INVALID_RECORD: i16 = 87;
/// The protocol's code for a transactional producer.
const TRANSACTIONAL_ID_AUTHORIZATION_FAILED: i16 = 53;

/// Answer a produce request.
pub(crate) async fn produce(
    shared: &Arc<Shared>,
    held: &mut Held,
    request: &Request<'_>,
) -> Vec<TopicResponse> {
    let mut answers = Vec::with_capacity(request.topics.len());
    for asked in &request.topics {
        let topic = names::topic(asked.name, shared.database.as_ref());
        let mut partitions = Vec::with_capacity(asked.partitions.len());
        for partition in &asked.partitions {
            let refused = |error_code: i16, message: Option<String>| PartitionResponse {
                index: partition.index,
                error_code,
                base_offset: -1,
                log_start_offset: -1,
                error_message: message,
            };
            let answer = if request.transactional_id.is_some() {
                refused(
                    TRANSACTIONAL_ID_AUTHORIZATION_FAILED,
                    Some("transactions are not served by this node".to_owned()),
                )
            } else {
                match (&topic, partition.index) {
                    (Some(topic), 0) => match partition.records.map(decode_batches).transpose() {
                        Err(crate::Error::Checksum { .. }) => refused(CORRUPT_MESSAGE, None),
                        Err(why) => refused(INVALID_RECORD, Some(why.to_string())),
                        Ok(batches) => {
                            write(
                                shared,
                                held,
                                topic,
                                asked.name,
                                partition.index,
                                &batches.unwrap_or_default(),
                            )
                            .await
                        }
                    },
                    _ => refused(errors::UNKNOWN_TOPIC_OR_PARTITION, None),
                }
            };
            partitions.push(answer);
        }
        answers.push(TopicResponse {
            name: asked.name.to_owned(),
            partitions,
        });
    }
    answers
}

async fn write(
    shared: &Arc<Shared>,
    held: &mut Held,
    topic: &Topic<'_>,
    asked: &str,
    index: i32,
    batches: &[Batch],
) -> PartitionResponse {
    let mut answer = PartitionResponse {
        index,
        error_code: errors::NONE,
        base_offset: -1,
        log_start_offset: -1,
        error_message: None,
    };
    for batch in batches {
        let idempotent = batch.producer_id >= 0;
        let key = (batch.producer_id, asked.to_owned(), index);
        let count = i32::try_from(batch.records.len()).unwrap_or(i32::MAX);
        if idempotent {
            match shared
                .producers
                .sequenced(&key, batch.producer_epoch, batch.base_sequence)
            {
                Sequenced::Fresh => {}
                Sequenced::Repeated(offset) => {
                    answer.base_offset = offset;
                    continue;
                }
                Sequenced::Duplicate => {
                    answer.error_code = errors::DUPLICATE_SEQUENCE_NUMBER;
                    continue;
                }
                Sequenced::OutOfOrder => {
                    answer.error_code = errors::OUT_OF_ORDER_SEQUENCE_NUMBER;
                    return answer;
                }
            }
        }
        match append(shared, held, topic, batch).await {
            Ok(base_offset) => {
                if answer.base_offset < 0 {
                    answer.base_offset = base_offset;
                }
                if idempotent {
                    shared.producers.written(
                        key,
                        batch.producer_epoch,
                        batch.base_sequence,
                        count,
                        base_offset,
                    );
                }
            }
            Err(failed) => {
                answer.error_code = code(&failed);
                match &failed {
                    Failed::Refused(refused) => answer.error_message = Some(refused.to_string()),
                    Failed::Carried(refused) => {
                        answer.error_message = Some(refused.message.clone())
                    }
                    Failed::Busy | Failed::Lost => {}
                }
                return answer;
            }
        }
    }
    answer
}

/// Append one batch's records, answering the offset the first one took.
async fn append(
    shared: &Arc<Shared>,
    held: &mut Held,
    topic: &Topic<'_>,
    batch: &Batch,
) -> Result<i64, Failed> {
    if batch.records.is_empty() {
        return Ok(-1);
    }
    let mut parameters = Parameters::new();
    let mut rows = String::new();
    for (at, record) in batch.records.iter().enumerate() {
        let headers = record
            .headers
            .iter()
            .map(|header| {
                Value::Object(
                    [
                        ("key".to_owned(), Value::String(header.key.clone())),
                        (
                            "value".to_owned(),
                            header.value.clone().map_or(Value::Null, Value::Bytes),
                        ),
                    ]
                    .into_iter()
                    .collect(),
                )
            })
            .collect();
        parameters.insert(
            format!("k{at}"),
            record.key.clone().map_or(Value::None, Value::Bytes),
        );
        parameters.insert(
            format!("v{at}"),
            record.value.clone().map_or(Value::Null, Value::Bytes),
        );
        parameters.insert(format!("h{at}"), Value::Array(headers));
        parameters.insert(format!("t{at}"), Value::Datetime(instant(record.timestamp)));
        if at > 0 {
            rows.push_str(", ");
        }
        rows.push_str(&format!("($k{at}, $v{at}, $h{at}, $t{at})"));
    }
    let script = format!(
        "INSERT INTO {} (key, value, headers, timestamp) VALUES {rows} RETURN POSITION;",
        topic.name
    );
    let outcomes = held
        .run(shared, topic.tenancy(), &script, &parameters)
        .await?;
    // The first position the batch took, as an offset; `-1` when the node
    // could not yet say (a message another node is still applying).
    let first = match outcomes.last() {
        Some(Outcome::Value(Value::Array(positions))) => {
            positions.iter().find_map(|position| match position {
                Value::Number(tessaridb::Number::Integer(held)) => Some(*held),
                _ => None,
            })
        }
        _ => None,
    };
    Ok(first.map_or(-1, |position| position.saturating_sub(1)))
}

/// A record's timestamp in milliseconds since the epoch, as an instant.
pub(crate) fn instant(millis: i64) -> Datetime {
    let seconds = millis.div_euclid(1000);
    let nanos = u32::try_from(millis.rem_euclid(1000).saturating_mul(1_000_000)).unwrap_or(0);
    Datetime::new(seconds, nanos).unwrap_or_else(|| Datetime::from_seconds(seconds))
}
