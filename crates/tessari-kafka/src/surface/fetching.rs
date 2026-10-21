//! Fetch: `READ FROM … AFTER` the offset, encoded as record batches.

use std::sync::Arc;

use tessari_constants::{KAFKA_FETCH_MESSAGES, KAFKA_MAX_FETCH_WAIT_MILLIS};
use tessaridb::{Outcome, Parameters, Value};

use crate::api::fetch::{PartitionAnswer, Request};
use crate::codec::Writer;
use crate::records::{Outgoing, OutgoingHeader, encode_batch};
use crate::surface::Shared;
use crate::surface::errors::{self, code};
use crate::surface::names;
use crate::surface::offsets::{bounds_of, partition_info, partition_parameter};
use crate::surface::running::Held;

/// A message, owned, in the shape a record batch needs.
struct Owned {
    offset: i64,
    timestamp: i64,
    key: Option<Vec<u8>>,
    value: Option<Vec<u8>>,
    headers: Vec<(String, Option<Vec<u8>>)>,
}

/// A Kafka record from a message the topic answered.
///
/// A message a Kafka client wrote carries its parts; any other — appended by a
/// statement — is sent as its JSON, with no key, so a Kafka consumer reads every
/// message of the topic and not only the ones a Kafka producer wrote.
fn record_of(position: i64, value: &Value) -> Owned {
    let offset = position.saturating_sub(1);
    if let Value::Object(fields) = value
        && let (Some(Value::Datetime(at)), Some(payload)) =
            (fields.get("timestamp"), fields.get("value"))
        && matches!(payload, Value::Bytes(_) | Value::Null)
    {
        let bytes = |held: Option<&Value>| match held {
            Some(Value::Bytes(bytes)) => Some(bytes.clone()),
            _ => None,
        };
        let headers = match fields.get("headers") {
            Some(Value::Array(items)) => items
                .iter()
                .filter_map(|item| match item {
                    Value::Object(header) => match header.get("key") {
                        Some(Value::String(key)) => Some((key.clone(), bytes(header.get("value")))),
                        _ => None,
                    },
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        };
        let timestamp = at
            .seconds()
            .checked_mul(1000)
            .and_then(|millis| millis.checked_add(i64::from(at.nanos() / 1_000_000)))
            .unwrap_or(-1);
        return Owned {
            offset,
            timestamp,
            key: bytes(fields.get("key")),
            value: bytes(Some(payload)),
            headers,
        };
    }
    let mut json = String::new();
    tessari_types::json::write(&mut json, value, &tessari_types::json::Names::new());
    Owned {
        offset,
        timestamp: -1,
        key: None,
        value: Some(json.into_bytes()),
        headers: Vec::new(),
    }
}

/// Encode `messages` as batches of consecutive offsets, stopping before
/// `budget` bytes once at least one batch is written.
fn encode(messages: &[Owned], budget: usize) -> crate::Result<Vec<u8>> {
    let mut writer = Writer::new();
    let mut start = 0;
    while start < messages.len() {
        let mut end = start.saturating_add(1);
        while end < messages.len()
            && messages[end].offset == messages[end.saturating_sub(1)].offset.saturating_add(1)
        {
            end = end.saturating_add(1);
        }
        let run = &messages[start..end];
        let headers: Vec<Vec<OutgoingHeader<'_>>> = run
            .iter()
            .map(|message| {
                message
                    .headers
                    .iter()
                    .map(|(key, value)| (key.as_str(), value.as_deref()))
                    .collect()
            })
            .collect();
        let outgoing: Vec<Outgoing<'_>> = run
            .iter()
            .zip(&headers)
            .map(|(message, headers)| Outgoing {
                timestamp: message.timestamp,
                key: message.key.as_deref(),
                value: message.value.as_deref(),
                headers,
            })
            .collect();
        let before = writer.len();
        encode_batch(&mut writer, run[0].offset, &outgoing)?;
        if before > 0 && writer.len() > budget {
            // Past the budget with something already sent: the client asks
            // again from where this answer ends.
            let mut trimmed = writer.into_bytes();
            trimmed.truncate(before);
            return Ok(trimmed);
        }
        start = end;
    }
    Ok(writer.into_bytes())
}

/// Answer a fetch request once, without waiting.
async fn once(
    shared: &Arc<Shared>,
    held: &mut Held,
    request: &Request<'_>,
) -> (Vec<(String, Vec<PartitionAnswer>)>, bool) {
    let mut answers = Vec::with_capacity(request.topics.len());
    let mut any = false;
    let mut left = usize::try_from(request.max_bytes.max(0)).unwrap_or(usize::MAX);
    for asked in &request.topics {
        let topic = names::topic(asked.name, shared.database.as_ref());
        let mut partitions = Vec::with_capacity(asked.partitions.len());
        for partition in &asked.partitions {
            let mut answer = PartitionAnswer {
                index: partition.index,
                error_code: errors::NONE,
                high_watermark: -1,
                log_start_offset: -1,
                records: None,
            };
            let Some(topic) = topic.as_ref() else {
                answer.error_code = errors::UNKNOWN_TOPIC_OR_PARTITION;
                partitions.push(answer);
                continue;
            };
            let script = format!(
                "INFO FOR TOPIC {0}; \
                 READ FROM {0} PARTITION $partition AFTER $after LIMIT {KAFKA_FETCH_MESSAGES};",
                topic.name
            );
            let after = Parameters::from([
                (
                    "after".to_owned(),
                    Value::Number(tessaridb::Number::Integer(partition.fetch_offset.max(0))),
                ),
                partition_parameter(partition.index),
            ]);
            match held.run(shared, topic.tenancy(), &script, &after).await {
                Err(failed) => answer.error_code = code(&failed),
                Ok(outcomes) => {
                    let bounds = outcomes
                        .iter()
                        .find_map(|outcome| match outcome {
                            Outcome::Value(info) => {
                                partition_info(info, partition.index).map(bounds_of)
                            }
                            _ => None,
                        })
                        .unwrap_or(crate::surface::offsets::Bounds {
                            log_start: 0,
                            high_watermark: 0,
                        });
                    answer.high_watermark = bounds.high_watermark;
                    answer.log_start_offset = bounds.log_start;
                    if partition.fetch_offset < bounds.log_start
                        || partition.fetch_offset > bounds.high_watermark
                    {
                        answer.error_code = errors::OFFSET_OUT_OF_RANGE;
                    } else {
                        let messages: Vec<Owned> = match outcomes.last() {
                            Some(Outcome::Records { records, .. }) => records
                                .iter()
                                .filter_map(|(_, message)| match message {
                                    Value::Object(fields) => {
                                        match (fields.get("position"), fields.get("value")) {
                                            (
                                                Some(Value::Number(tessaridb::Number::Integer(
                                                    position,
                                                ))),
                                                Some(value),
                                            ) => Some(record_of(*position, value)),
                                            _ => None,
                                        }
                                    }
                                    _ => None,
                                })
                                .collect(),
                            _ => Vec::new(),
                        };
                        let budget = left
                            .min(usize::try_from(partition.max_bytes.max(1)).unwrap_or(usize::MAX));
                        match encode(&messages, budget) {
                            Ok(bytes) => {
                                any |= !bytes.is_empty();
                                left = left.saturating_sub(bytes.len());
                                answer.records = Some(bytes);
                            }
                            Err(_) => answer.error_code = errors::UNKNOWN_SERVER_ERROR,
                        }
                    }
                }
            }
            partitions.push(answer);
        }
        answers.push((asked.name.to_owned(), partitions));
    }
    (answers, any)
}

/// Answer a fetch request, waiting up to its `max_wait_ms` for a message when
/// none is there yet — woken by the store's own commit signal, not by polling.
pub(crate) async fn fetch(
    shared: &Arc<Shared>,
    held: &mut Held,
    request: &Request<'_>,
) -> Vec<(String, Vec<PartitionAnswer>)> {
    let wait = u64::try_from(request.max_wait_ms.max(0))
        .unwrap_or(0)
        .min(KAFKA_MAX_FETCH_WAIT_MILLIS);
    let now = tokio::time::Instant::now();
    let deadline = now
        .checked_add(std::time::Duration::from_millis(wait))
        .unwrap_or(now);
    let mut commits = shared.db.commits().watching();
    loop {
        let (answers, any) = once(shared, held, request).await;
        if any || request.min_bytes <= 0 || shared.stopping.asked() {
            return answers;
        }
        match tokio::time::timeout_at(deadline, commits.changed()).await {
            Ok(Ok(())) => {}
            _ => return answers,
        }
    }
}
