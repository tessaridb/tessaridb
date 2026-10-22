//! The stream commands carried out against a topic (ADR-0133).
//!
//! An entry's ID is `<appended ms>-<position>`: positions are dense and
//! instants never go back along a topic, so the IDs rise with the positions
//! and an ID names a position by two reads of `READ … SINCE` — the first
//! message of its millisecond and the first of the next — rather than by a
//! walk. An entry appended before its store kept instants answers `0-<p>`.

use std::collections::BTreeMap;

use tessari_types::{Datetime, Number};
use tessaridb::{Outcome, Parameters, Value};

use crate::codec::Reply;
use crate::surface::commands::{Id, Since, Stream, Trim, now_millis};
use crate::surface::procedures::Against;
use crate::surface::replies;
use crate::surface::running::Failed;
use crate::surface::statement::value_of;
use crate::surface::values;

/// How many entries one read of a topic asks for.
const PAGE: u64 = tessari_constants::RESP_SCAN_PAGE;

/// What `INFO FOR TOPIC` says of a stream.
struct Held {
    /// The first position still held, when any is.
    first: Option<u64>,
    /// The last position given, held or not.
    last: u64,
    groups: usize,
}

/// One entry as it was read.
struct Entry {
    position: u64,
    ms: u64,
    value: Value,
}

impl Entry {
    fn id(&self) -> Id {
        Id {
            ms: self.ms,
            seq: self.position,
        }
    }

    /// `[id, [field, value, …]]`.
    fn reply(&self) -> Reply {
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

/// A field's value as the bytes a client is given.
fn text(value: &Value) -> Reply {
    Reply::Bulk(values::rendered(value).unwrap_or_else(|| value.to_string().into_bytes()))
}

fn whole(value: &Value) -> Option<u64> {
    match value {
        Value::Number(Number::Integer(integer)) => u64::try_from(*integer).ok(),
        _ => None,
    }
}

fn integer(value: u64) -> Value {
    Value::Number(Number::Integer(i64::try_from(value).unwrap_or(i64::MAX)))
}

fn reply_count(count: u64) -> Reply {
    Reply::Integer(i64::try_from(count).unwrap_or(i64::MAX))
}

/// A refusal as its reply: a key that is not a topic is the wrong type.
fn refused(failed: &Failed) -> Reply {
    match failed {
        Failed::Refused(tessaridb::Error::NotATopic { .. }) => replies::wrong_type(),
        failed => replies::failed(failed),
    }
}

/// Whether `failed` says the topic is not there.
fn missing(failed: &Failed) -> bool {
    matches!(
        failed,
        Failed::Refused(tessaridb::Error::Unknown {
            entity: "table",
            ..
        })
    )
}

impl Against<'_> {
    /// Carry out a stream command, `XREAD … BLOCK` included as its one try.
    pub(crate) async fn stream(&mut self, stream: &Stream) -> Reply {
        let answered = match stream {
            Stream::Add {
                key,
                make,
                trim,
                fields,
            } => self.add(key, *make, *trim, fields).await,
            Stream::Len(key) => self
                .held(key)
                .await
                .map(|held| reply_count(held.map_or(0, |held| length(&held)))),
            Stream::Trim { key, trim } => self.trim(key, *trim).await.map(reply_count),
            Stream::Range {
                key,
                from,
                to,
                count,
                reverse,
            } => self
                .range(key, (*from, *to), *count, *reverse)
                .await
                .map(|entries| Reply::Array(entries.iter().map(Entry::reply).collect())),
            Stream::Read { count, keys, .. } => self.read(keys, *count).await,
            Stream::Info(key) => self.info(key).await,
        };
        answered.unwrap_or_else(|reply| reply)
    }

    /// The ID `$` stands for on `key` now: the last entry's, so that only
    /// entries added after it are past it. A topic holding nothing it gave
    /// stands at the present millisecond, which the next entry's instant
    /// cannot be below on this node's clock.
    pub(crate) async fn newest(&mut self, key: &str) -> Result<Id, Reply> {
        let Some(held) = self.held(key).await? else {
            return Ok(Id::LEAST);
        };
        if held.last == 0 {
            return Ok(Id::LEAST);
        }
        if let Some(entry) = self
            .entries(key, held.last.saturating_sub(1), 1)
            .await?
            .pop()
            && entry.position == held.last
        {
            return Ok(entry.id());
        }
        Ok(Id {
            ms: u64::try_from(now_millis()).unwrap_or(0),
            seq: held.last,
        })
    }

    /// One statement's outcome.
    async fn one(&mut self, statement: String, parameters: Parameters) -> Result<Outcome, Failed> {
        self.run(&[statement], &parameters)
            .await
            .map(|mut outcomes| outcomes.pop().unwrap_or(Outcome::Done))
    }

    /// What the topic `key` holds, or `None` when there is no such topic.
    async fn held(&mut self, key: &str) -> Result<Option<Held>, Reply> {
        let fields = match self
            .one(format!("INFO FOR TOPIC {key}"), Parameters::new())
            .await
        {
            Ok(outcome) => match value_of(outcome) {
                Value::Object(fields) => fields,
                _ => return Err(replies::syntax()),
            },
            Err(failed) if missing(&failed) => return Ok(None),
            Err(failed) => return Err(refused(&failed)),
        };
        let at = |name: &str| fields.get(name).and_then(whole);
        Ok(Some(Held {
            first: at("first"),
            last: at("last").unwrap_or(0),
            groups: match fields.get("groups") {
                Some(Value::Object(groups)) => groups.len(),
                _ => 0,
            },
        }))
    }

    /// Up to `limit` entries after `position`, in order.
    async fn entries(&mut self, key: &str, after: u64, limit: u64) -> Result<Vec<Entry>, Reply> {
        let mut parameters = Parameters::new();
        parameters.insert("after".to_owned(), integer(after));
        parameters.insert("limit".to_owned(), integer(limit));
        let outcome = self
            .one(
                format!("READ FROM {key} AFTER $after LIMIT $limit"),
                parameters,
            )
            .await
            .map_err(|failed| refused(&failed))?;
        let Outcome::Records { records, .. } = outcome else {
            return Ok(Vec::new());
        };
        Ok(records
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
            .collect())
    }

    /// The first position appended at or after millisecond `ms`, or `None`.
    async fn since(&mut self, key: &str, ms: u64) -> Result<Option<u64>, Reply> {
        let Some(at) = instant(ms) else {
            return Ok(None);
        };
        let mut parameters = Parameters::new();
        parameters.insert("at".to_owned(), Value::Datetime(at));
        let outcome = self
            .one(format!("READ FROM {key} SINCE $at LIMIT 1"), parameters)
            .await
            .map_err(|failed| refused(&failed))?;
        let Outcome::Records { records, .. } = outcome else {
            return Ok(None);
        };
        Ok(records
            .into_iter()
            .next()
            .and_then(|(_, record)| match record {
                Value::Object(fields) => fields.get("position").and_then(whole),
                _ => None,
            }))
    }

    /// The first position whose entry's ID is at least `id`; one past the last
    /// when there is none.
    async fn first_at(&mut self, key: &str, held: &Held, id: Id) -> Result<u64, Reply> {
        let past = held.last.saturating_add(1);
        let Some(first) = held.first else {
            return Ok(past);
        };
        // Below this millisecond's first message every ID is smaller; from the
        // next millisecond's first, every ID is larger.
        let band = if id.ms == 0 {
            first
        } else {
            self.since(key, id.ms).await?.unwrap_or(past)
        };
        let beyond = match id.ms.checked_add(1) {
            Some(next) => self.since(key, next).await?.unwrap_or(past),
            None => past,
        };
        let candidate = band.max(id.seq).max(first);
        Ok(if candidate < beyond {
            candidate
        } else {
            beyond
        })
    }

    /// The entries from position `from` to `to`, both included, at most
    /// `count` of them from the start.
    async fn span(
        &mut self,
        key: &str,
        (from, to): (u64, u64),
        count: Option<u64>,
    ) -> Result<Vec<Entry>, Reply> {
        let mut found = Vec::new();
        let mut after = from.saturating_sub(1);
        let mut wanted = count.unwrap_or(u64::MAX);
        while after < to && wanted > 0 {
            let page = PAGE.min(to.saturating_sub(after)).min(wanted);
            let read = self.entries(key, after, page).await?;
            let Some(last) = read.last().map(|entry| entry.position) else {
                break;
            };
            for entry in read.into_iter().filter(|entry| entry.position <= to) {
                wanted = wanted.saturating_sub(1);
                found.push(entry);
            }
            after = last;
        }
        Ok(found)
    }

    async fn range(
        &mut self,
        key: &str,
        (from, to): (Id, Id),
        count: Option<u64>,
        reverse: bool,
    ) -> Result<Vec<Entry>, Reply> {
        let Some(held) = self.held(key).await? else {
            return Ok(Vec::new());
        };
        if from > to || count == Some(0) {
            return Ok(Vec::new());
        }
        let low = self.first_at(key, &held, from).await?;
        let high = match to.next() {
            Some(next) => self.first_at(key, &held, next).await?.saturating_sub(1),
            None => held.last,
        };
        if low > high || low == 0 {
            return Ok(Vec::new());
        }
        if !reverse {
            return self.span(key, (low, high), count).await;
        }
        // The newest `count` of them, newest first.
        let start = match count {
            Some(count) => low.max(high.saturating_sub(count.saturating_sub(1))),
            None => low,
        };
        let mut entries = self.span(key, (start, high), None).await?;
        entries.reverse();
        if let Some(count) = count.and_then(|count| usize::try_from(count).ok()) {
            entries.truncate(count);
        }
        Ok(entries)
    }

    async fn add(
        &mut self,
        key: &str,
        make: bool,
        trim: Option<Trim>,
        fields: &[(String, Vec<u8>)],
    ) -> Result<Reply, Reply> {
        let body: BTreeMap<String, Value> = fields
            .iter()
            .map(|(field, value)| (field.clone(), values::stored(value)))
            .collect();
        let append = format!("CREATE {key} = $body RETURN POSITION");
        let mut parameters = Parameters::new();
        parameters.insert("body".to_owned(), Value::Object(body));
        let outcome = match self.one(append.clone(), parameters.clone()).await {
            Ok(outcome) => outcome,
            Err(failed) if missing(&failed) && !make => return Ok(Reply::Nil),
            Err(failed) if missing(&failed) => {
                self.one(
                    format!("DEFINE TOPIC IF NOT EXISTS {key}"),
                    Parameters::new(),
                )
                .await
                .map_err(|failed| refused(&failed))?;
                self.one(append, parameters)
                    .await
                    .map_err(|failed| refused(&failed))?
            }
            Err(failed) => return Err(refused(&failed)),
        };
        let position = match value_of(outcome) {
            Value::Array(positions) => positions.first().and_then(whole),
            _ => None,
        }
        .ok_or_else(replies::syntax)?;
        let ms = self
            .entries(key, position.saturating_sub(1), 1)
            .await?
            .into_iter()
            .find(|entry| entry.position == position)
            .map_or(0, |entry| entry.ms);
        if let Some(trim) = trim {
            self.trim(key, trim).await?;
        }
        Ok(Reply::text(Id { ms, seq: position }.text()))
    }

    /// Trim `key`'s front, answering how many entries went.
    async fn trim(&mut self, key: &str, trim: Trim) -> Result<u64, Reply> {
        let Some(held) = self.held(key).await? else {
            return Ok(0);
        };
        let mut parameters = Parameters::new();
        let statement = match trim {
            Trim::Keep(kept) => {
                parameters.insert("kept".to_owned(), integer(kept));
                format!("TRIM {key} KEEP $kept")
            }
            Trim::Below(id) => {
                let below = self.first_at(key, &held, id).await?;
                parameters.insert("below".to_owned(), integer(below));
                format!("TRIM {key} BEFORE $below")
            }
        };
        let outcome = self
            .one(statement, parameters)
            .await
            .map_err(|failed| refused(&failed))?;
        Ok(whole(&value_of(outcome)).unwrap_or(0))
    }

    async fn read(&mut self, keys: &[(String, Since)], count: Option<u64>) -> Result<Reply, Reply> {
        let mut answered = Vec::new();
        for (key, since) in keys {
            let Some(held) = self.held(key).await? else {
                continue;
            };
            let from = match since {
                Since::Newest => continue,
                Since::Last => match held.first {
                    Some(_) => held.last,
                    None => continue,
                },
                Since::After(id) => match id.next() {
                    Some(next) => self.first_at(key, &held, next).await?,
                    None => continue,
                },
            };
            let entries = self.span(key, (from, held.last), count).await?;
            if !entries.is_empty() {
                answered.push((
                    Reply::text(key.as_bytes()),
                    Reply::Array(entries.iter().map(Entry::reply).collect()),
                ));
            }
        }
        Ok(if answered.is_empty() {
            Reply::NilArray
        } else {
            Reply::Keyed(answered)
        })
    }

    async fn info(&mut self, key: &str) -> Result<Reply, Reply> {
        let held = self
            .held(key)
            .await?
            .ok_or_else(|| Reply::error("ERR no such key"))?;
        let (head, tail) = match held.first {
            Some(first) => (
                self.entries(key, first.saturating_sub(1), 1).await?.pop(),
                self.entries(key, held.last.saturating_sub(1), 1)
                    .await?
                    .pop(),
            ),
            None => (None, None),
        };
        let given = Id {
            ms: 0,
            seq: held.last,
        };
        let last_id = tail.as_ref().map_or(given, Entry::id);
        let first_id = head.as_ref().map_or(Id::LEAST, Entry::id);
        let entry = |found: Option<&Entry>| found.map_or(Reply::Nil, Entry::reply);
        Ok(Reply::Map(vec![
            (Reply::text("length"), reply_count(length(&held))),
            (
                Reply::text("last-generated-id"),
                Reply::text(last_id.text()),
            ),
            (Reply::text("max-deleted-entry-id"), Reply::text("0-0")),
            (Reply::text("entries-added"), reply_count(held.last)),
            (
                Reply::text("recorded-first-entry-id"),
                Reply::text(first_id.text()),
            ),
            (
                Reply::text("groups"),
                Reply::Integer(i64::try_from(held.groups).unwrap_or(i64::MAX)),
            ),
            (Reply::text("first-entry"), entry(head.as_ref())),
            (Reply::text("last-entry"), entry(tail.as_ref())),
        ]))
    }
}

/// How many entries a topic holds.
fn length(held: &Held) -> u64 {
    held.first
        .map_or(0, |first| held.last.saturating_sub(first).saturating_add(1))
}

fn instant(ms: u64) -> Option<Datetime> {
    Datetime::new(
        i64::try_from(ms / 1000).ok()?,
        u32::try_from((ms % 1000).checked_mul(1_000_000)?).ok()?,
    )
}

fn millis(at: Datetime) -> u64 {
    u64::try_from(at.seconds())
        .ok()
        .and_then(|seconds| seconds.checked_mul(1000))
        .and_then(|ms| ms.checked_add(u64::from(at.nanos() / 1_000_000)))
        .unwrap_or(0)
}
