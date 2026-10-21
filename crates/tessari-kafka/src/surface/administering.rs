//! CreateTopics and DeleteTopics as `DEFINE TOPIC` and `DROP TOPIC` through
//! the caller's session, so the store's grants decide who may do either.

use std::fmt::Write as _;
use std::sync::Arc;

use tessaridb::Parameters;

use crate::api::topics::{Creatable, CreateRequest, Created};
use crate::surface::Shared;
use crate::surface::errors::{self, code};
use crate::surface::names::{self, Topic};
use crate::surface::running::{Failed, Held};

/// Settings a broker takes that mean nothing for a store's topic — segment
/// files, replica durability, compression on disk — accepted and not kept.
const IGNORED: &[&str] = &[
    "compression.type",
    "file.delete.delay.ms",
    "flush.messages",
    "flush.ms",
    "index.interval.bytes",
    "min.insync.replicas",
    "preallocate",
    "segment.bytes",
    "segment.index.bytes",
    "segment.jitter.ms",
    "segment.ms",
    "unclean.leader.election.enable",
];

fn refused(name: &str, code: i16, message: impl Into<String>) -> Created {
    Created {
        name: name.to_owned(),
        error_code: code,
        message: Some(message.into()),
    }
}

/// A positive whole number of a setting, `None` for `-1` (no limit).
fn limit(key: &str, value: Option<&str>) -> Result<Option<i64>, String> {
    let parsed = value
        .and_then(|value| value.trim().parse::<i64>().ok())
        .ok_or_else(|| format!("{key} must be a whole number"))?;
    match parsed {
        -1 => Ok(None),
        positive if positive > 0 => Ok(Some(positive)),
        _ => Err(format!("{key} must be positive, or -1 for no limit")),
    }
}

/// The `DEFINE TOPIC` a request stands for, or why it cannot be one.
fn definition(asked: &Creatable<'_>, topic: &Topic<'_>) -> Result<String, Created> {
    if !matches!(asked.partitions, -1 | 1) {
        return Err(refused(
            asked.name,
            errors::INVALID_PARTITIONS,
            "a topic here is one partition",
        ));
    }
    if !matches!(asked.replication, -1 | 1) {
        return Err(refused(
            asked.name,
            errors::INVALID_REPLICATION_FACTOR,
            "replication is the store's, declared on the namespace",
        ));
    }
    if !asked.assignments.iter().all(|(partition, brokers)| {
        *partition == 0
            && brokers
                .iter()
                .all(|broker| *broker == super::metadata::NODE_ID)
    }) || asked.assignments.len() > 1
    {
        return Err(refused(
            asked.name,
            errors::INVALID_PARTITIONS,
            "a topic here is one partition, on this node",
        ));
    }
    let mut statement = format!("DEFINE TOPIC {}", topic.name);
    for (key, value) in &asked.configs {
        let bad = |why: String| refused(asked.name, errors::INVALID_CONFIG, why);
        match *key {
            "retention.ms" => {
                if let Some(millis) = limit(key, *value).map_err(bad)? {
                    let _ = write!(statement, " RETAIN {millis}ms");
                }
            }
            "retention.bytes" => {
                if let Some(bytes) = limit(key, *value).map_err(bad)? {
                    let _ = write!(statement, " RETAIN BYTES {bytes}");
                }
            }
            "max.message.bytes" => {
                if let Some(bytes) = limit(key, *value).map_err(bad)? {
                    let _ = write!(statement, " MAX BYTES {bytes}");
                }
            }
            "cleanup.policy" if *value == Some("delete") => {}
            "cleanup.policy" => {
                return Err(bad(
                    "a topic here is never compacted; a newest value per key is a space".to_owned(),
                ));
            }
            ignored if IGNORED.contains(&ignored) => {}
            unknown => {
                return Err(bad(format!(
                    "{unknown} is not a setting a topic here takes"
                )));
            }
        }
    }
    statement.push(';');
    Ok(statement)
}

async fn create_one(
    shared: &Arc<Shared>,
    held: &mut Held,
    asked: &Creatable<'_>,
    validate_only: bool,
) -> Created {
    let Some(topic) = names::topic(asked.name, shared.database.as_ref()) else {
        return refused(
            asked.name,
            errors::INVALID_TOPIC_EXCEPTION,
            "a topic is <namespace>.<database>.<topic>, each part an identifier",
        );
    };
    let statement = match definition(asked, &topic) {
        Ok(statement) => statement,
        Err(refusal) => return refusal,
    };
    let script = if validate_only {
        // Nothing is defined: asking for the topic says whether it exists,
        // and a refusal other than "no such topic" is the caller's.
        format!("INFO FOR TOPIC {};", topic.name)
    } else {
        statement
    };
    let answered = held
        .run(shared, topic.tenancy(), &script, &Parameters::new())
        .await;
    let error_code = match (&answered, validate_only) {
        (Ok(_), false) => errors::NONE,
        (Ok(_), true) => errors::TOPIC_ALREADY_EXISTS,
        (Err(failed), true) if code(failed) == errors::UNKNOWN_TOPIC_OR_PARTITION => errors::NONE,
        (Err(failed), _) => created_code(failed),
    };
    Created {
        name: asked.name.to_owned(),
        error_code,
        message: answered.err().and_then(|failed| match failed {
            Failed::Refused(refused) if error_code != errors::NONE => Some(refused.to_string()),
            _ => None,
        }),
    }
}

/// A failed definition's code: a name already taken is the protocol's own.
fn created_code(failed: &Failed) -> i16 {
    match failed {
        Failed::Refused(tessaridb::Error::Store(tessaridb::StoreError::NameTaken { .. })) => {
            errors::TOPIC_ALREADY_EXISTS
        }
        other => code(other),
    }
}

/// Create each topic asked for; an answer per topic.
pub(crate) async fn create(
    shared: &Arc<Shared>,
    held: &mut Held,
    request: &CreateRequest<'_>,
) -> Vec<Created> {
    let mut created = Vec::with_capacity(request.topics.len());
    for asked in &request.topics {
        created.push(create_one(shared, held, asked, request.validate_only).await);
    }
    created
}

/// Drop each topic asked for; a code per topic.
pub(crate) async fn delete(shared: &Arc<Shared>, held: &mut Held, asked: &[&str]) -> Vec<i16> {
    let mut codes = Vec::with_capacity(asked.len());
    for name in asked {
        let answered = match names::topic(name, shared.database.as_ref()) {
            None => errors::UNKNOWN_TOPIC_OR_PARTITION,
            Some(topic) => {
                let script = format!("DROP TOPIC {};", topic.name);
                match held
                    .run(shared, topic.tenancy(), &script, &Parameters::new())
                    .await
                {
                    Ok(_) => errors::NONE,
                    Err(failed) => code(&failed),
                }
            }
        };
        codes.push(answered);
    }
    codes
}
