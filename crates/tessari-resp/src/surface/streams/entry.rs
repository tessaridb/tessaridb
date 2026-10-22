//! An entry as a stream command reads and answers it, and the numbers a
//! topic's description is read as.

use tessari_types::{Datetime, Number};
use tessaridb::{Outcome, Value};

use crate::codec::Reply;
use crate::surface::commands::Id;
use crate::surface::replies;
use crate::surface::running::Failed;
use crate::surface::values;

/// How many entries one read of a topic asks for.
pub(crate) const PAGE: u64 = tessari_constants::RESP_SCAN_PAGE;

/// What `INFO FOR TOPIC` says of a stream.
pub(crate) struct Held {
    /// The first position still held, when any is.
    pub(crate) first: Option<u64>,
    /// The last position given, held or not.
    pub(crate) last: u64,
    pub(crate) groups: usize,
}

/// One entry as it was read.
pub(crate) struct Entry {
    pub(crate) position: u64,
    pub(crate) ms: u64,
    pub(crate) value: Value,
}

impl Entry {
    pub(crate) fn id(&self) -> Id {
        Id {
            ms: self.ms,
            seq: self.position,
        }
    }

    /// `[id, [field, value, …]]`.
    pub(crate) fn reply(&self) -> Reply {
        let fields = match &self.value {
            Value::Object(fields) => fields
                .iter()
                .flat_map(|(field, value)| [Reply::text(field.as_bytes()), text(value)])
                .collect(),
            // A message written from TessariQL need not be an object.
            other => vec![Reply::text("value"), text(other)],
        };
        Reply::Array(vec![Reply::text(self.id().text()), Reply::Array(fields)])
    }
}

/// The entries a topic read answered, a group's hand-out included.
pub(crate) fn read(outcome: Outcome) -> Vec<Entry> {
    let Outcome::Records { records, .. } = outcome else {
        return Vec::new();
    };
    records
        .into_iter()
        .filter_map(|(_, record)| {
            let Value::Object(mut fields) = record else {
                return None;
            };
            let position = fields.get("position").and_then(whole)?;
            let ms = match fields.get("appended") {
                Some(Value::Datetime(at)) => millis(*at),
                _ => 0,
            };
            Some(Entry {
                position,
                ms,
                value: fields.remove("value").unwrap_or(Value::None),
            })
        })
        .collect()
}

/// A field's value as the bytes a client is given.
pub(crate) fn text(value: &Value) -> Reply {
    Reply::Bulk(values::rendered(value).unwrap_or_else(|| value.to_string().into_bytes()))
}

pub(crate) fn whole(value: &Value) -> Option<u64> {
    match value {
        Value::Number(Number::Integer(integer)) => u64::try_from(*integer).ok(),
        _ => None,
    }
}

pub(crate) fn integer(value: u64) -> Value {
    Value::Number(Number::Integer(i64::try_from(value).unwrap_or(i64::MAX)))
}

pub(crate) fn reply_count(count: u64) -> Reply {
    Reply::Integer(i64::try_from(count).unwrap_or(i64::MAX))
}

/// A refusal as its reply: a key that is not a topic is the wrong type.
pub(crate) fn refused(failed: &Failed) -> Reply {
    match failed {
        Failed::Refused(tessaridb::Error::NotATopic { .. }) => replies::wrong_type(),
        failed => replies::failed(failed),
    }
}

/// Whether `failed` says the topic is not there.
pub(crate) fn missing(failed: &Failed) -> bool {
    matches!(
        failed,
        Failed::Refused(tessaridb::Error::Unknown {
            entity: "table",
            ..
        })
    )
}

/// How many entries a topic holds.
pub(crate) fn length(held: &Held) -> u64 {
    held.first
        .map_or(0, |first| held.last.saturating_sub(first).saturating_add(1))
}

pub(crate) fn instant(ms: u64) -> Option<Datetime> {
    Datetime::new(
        i64::try_from(ms / 1000).ok()?,
        u32::try_from((ms % 1000).checked_mul(1_000_000)?).ok()?,
    )
}

pub(crate) fn millis(at: Datetime) -> u64 {
    u64::try_from(at.seconds())
        .ok()
        .and_then(|seconds| seconds.checked_mul(1000))
        .and_then(|ms| ms.checked_add(u64::from(at.nanos() / 1_000_000)))
        .unwrap_or(0)
}
