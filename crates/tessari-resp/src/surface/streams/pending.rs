//! What a stream group holds, read from `INFO FOR GROUP`, and handed on with
//! `HAND` (ADR-0133).
//!
//! An entry's idle time is how long since it was last delivered, and a
//! member's since it last read under the group, both by this node's clock
//! against the instants the group kept.

use tessaridb::{Parameters, Value};

use super::entry::{self, Entry, integer, millis, missing, refused, whole};
use super::groups::{no_group, not_a_group};
use crate::codec::Reply;
use crate::surface::commands::{Claimed, Id, Pending, now_millis};
use crate::surface::procedures::Against;
use crate::surface::statement::value_of;

/// One held entry, as the group describes it.
struct Flight {
    id: Id,
    member: String,
    delivered: u64,
    deliveries: u64,
}

/// One member, as the group describes it.
struct Member {
    name: String,
    held: u64,
    seen: u64,
    active: u64,
}

/// What `INFO FOR GROUP` says.
struct Described {
    flight: Vec<Flight>,
    members: Vec<Member>,
    position: u64,
}

/// An instant field as milliseconds since the epoch; 0 when absent. The map
/// is the store's object value as `INFO FOR GROUP` answered it, read once.
fn instant(fields: &std::collections::BTreeMap<String, Value>, name: &str) -> u64 {
    match fields.get(name) {
        Some(Value::Datetime(at)) => millis(*at),
        _ => 0,
    }
}

fn text_of(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.clone(),
        _ => String::new(),
    }
}

/// Milliseconds since `then`, by this node's clock.
fn since(then: u64) -> Reply {
    let now = u64::try_from(now_millis()).unwrap_or(0);
    Reply::Integer(i64::try_from(now.saturating_sub(then)).unwrap_or(i64::MAX))
}

fn count_reply(count: u64) -> Reply {
    Reply::Integer(i64::try_from(count).unwrap_or(i64::MAX))
}

fn described(value: Value) -> Described {
    let Value::Object(fields) = value else {
        return Described {
            flight: Vec::new(),
            members: Vec::new(),
            position: 0,
        };
    };
    let flight = match fields.get("held") {
        Some(Value::Array(held)) => held
            .iter()
            .filter_map(|held| match held {
                Value::Object(held) => Some(Flight {
                    id: Id {
                        ms: instant(held, "appended"),
                        seq: held.get("position").and_then(whole)?,
                    },
                    member: text_of(held.get("member")),
                    delivered: instant(held, "delivered"),
                    deliveries: held.get("deliveries").and_then(whole).unwrap_or(0),
                }),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    };
    let members = match fields.get("members") {
        Some(Value::Object(members)) => members
            .iter()
            .map(|(name, member)| match member {
                Value::Object(member) => Member {
                    name: name.clone(),
                    held: member.get("held").and_then(whole).unwrap_or(0),
                    seen: instant(member, "seen"),
                    active: instant(member, "active"),
                },
                _ => Member {
                    name: name.clone(),
                    held: 0,
                    seen: 0,
                    active: 0,
                },
            })
            .collect(),
        _ => Vec::new(),
    };
    Described {
        flight,
        members,
        position: fields.get("position").and_then(whole).unwrap_or(0),
    }
}

impl Against<'_> {
    /// Carry out a question about what a group holds, or a hand-off of it.
    pub(crate) async fn pending(&mut self, pending: &Pending) -> Reply {
        let answered = match pending {
            Pending::Summary { key, group } => self.summary(key, group).await,
            Pending::Listed {
                key,
                group,
                idle,
                from,
                to,
                count,
                consumer,
            } => {
                self.listed(
                    (key, group),
                    *idle,
                    (*from, *to),
                    *count,
                    consumer.as_deref(),
                )
                .await
            }
            Pending::Claim {
                key,
                group,
                consumer,
                idle,
                which,
                ids,
            } => self.claim((key, group, consumer), *idle, which, *ids).await,
            Pending::Groups(key) => self.groups(key).await,
            Pending::Members { key, group } => self.members(key, group).await,
        };
        answered.unwrap_or_else(|reply| reply)
    }

    async fn describe(&mut self, key: &str, group: &str) -> Result<Described, Reply> {
        let mut parameters = Parameters::new();
        parameters.insert("group".to_owned(), Value::from(group));
        match self
            .one(format!("INFO FOR GROUP $group ON TOPIC {key}"), parameters)
            .await
        {
            Ok(outcome) => Ok(described(value_of(outcome))),
            Err(failed) if missing(&failed) || not_a_group(&failed) => Err(no_group(key, group)),
            Err(failed) => Err(refused(&failed)),
        }
    }

    async fn summary(&mut self, key: &str, group: &str) -> Result<Reply, Reply> {
        let described = self.describe(key, group).await?;
        let (Some(least), Some(most)) = (
            described.flight.iter().map(|held| held.id).min(),
            described.flight.iter().map(|held| held.id).max(),
        ) else {
            return Ok(Reply::Array(vec![
                Reply::Integer(0),
                Reply::Nil,
                Reply::Nil,
                Reply::NilArray,
            ]));
        };
        let holders = described
            .members
            .iter()
            .filter(|member| member.held > 0)
            .map(|member| {
                Reply::Array(vec![
                    Reply::text(member.name.as_bytes()),
                    Reply::text(member.held.to_string()),
                ])
            })
            .collect();
        Ok(Reply::Array(vec![
            count_reply(u64::try_from(described.flight.len()).unwrap_or(u64::MAX)),
            Reply::text(least.text()),
            Reply::text(most.text()),
            Reply::Array(holders),
        ]))
    }

    async fn listed(
        &mut self,
        (key, group): (&str, &str),
        idle: u64,
        (from, to): (Id, Id),
        count: u64,
        consumer: Option<&str>,
    ) -> Result<Reply, Reply> {
        let mut described = self.describe(key, group).await?;
        described.flight.sort_by_key(|held| held.id);
        let now = u64::try_from(now_millis()).unwrap_or(0);
        let rows = described
            .flight
            .iter()
            .filter(|held| held.id >= from && held.id <= to)
            .filter(|held| consumer.is_none_or(|consumer| held.member == consumer))
            .filter(|held| now.saturating_sub(held.delivered) >= idle)
            .take(usize::try_from(count).unwrap_or(usize::MAX))
            .map(|held| {
                Reply::Array(vec![
                    Reply::text(held.id.text()),
                    Reply::text(held.member.as_bytes()),
                    since(held.delivered),
                    count_reply(held.deliveries),
                ])
            })
            .collect();
        Ok(Reply::Array(rows))
    }

    async fn claim(
        &mut self,
        (key, group, consumer): (&str, &str, &str),
        idle: u64,
        which: &Claimed,
        ids: bool,
    ) -> Result<Reply, Reply> {
        let mut parameters = Parameters::new();
        parameters.insert("group".to_owned(), Value::from(group));
        parameters.insert("member".to_owned(), Value::from(consumer));
        let reach = match which {
            Claimed::At(positions) => {
                parameters.insert(
                    "positions".to_owned(),
                    Value::Array(positions.iter().map(|at| integer(*at)).collect()),
                );
                "AT $positions".to_owned()
            }
            Claimed::After { after, count } => {
                parameters.insert("after".to_owned(), integer(*after));
                parameters.insert("limit".to_owned(), integer(*count));
                "AFTER $after LIMIT $limit".to_owned()
            }
        };
        let mut statement = format!("HAND {key} FOR CONSUMER $group {reach} TO $member");
        if idle > 0 {
            let seconds = i64::try_from(idle / 1000).unwrap_or(i64::MAX);
            let nanos = u32::try_from((idle % 1000).saturating_mul(1_000_000)).unwrap_or(0);
            if let Some(span) = tessari_types::Duration::new(seconds, nanos) {
                parameters.insert("idle".to_owned(), Value::Duration(span));
                statement.push_str(" IF IDLE $idle");
            }
        }
        let entries = match self.one(statement, parameters).await {
            Ok(outcome) => entry::read(outcome),
            Err(failed) if missing(&failed) || not_a_group(&failed) => {
                return Err(no_group(key, group));
            }
            Err(failed) => return Err(refused(&failed)),
        };
        let answered = Reply::Array(
            entries
                .iter()
                .map(|entry| {
                    if ids {
                        Reply::text(entry.id().text())
                    } else {
                        entry.reply()
                    }
                })
                .collect(),
        );
        let Claimed::After { count, .. } = which else {
            return Ok(answered);
        };
        // A full page may have more behind it; the next scan starts past it.
        let next = match entries.last() {
            Some(last) if u64::try_from(entries.len()).unwrap_or(u64::MAX) >= *count => Id {
                ms: 0,
                seq: last.position.saturating_add(1),
            },
            _ => Id::LEAST,
        };
        Ok(Reply::Array(vec![
            Reply::text(next.text()),
            answered,
            Reply::Array(Vec::new()),
        ]))
    }

    async fn groups(&mut self, key: &str) -> Result<Reply, Reply> {
        let outcome = match self
            .one(format!("INFO FOR TOPIC {key}"), Parameters::new())
            .await
        {
            Ok(outcome) => outcome,
            Err(failed) if missing(&failed) => return Err(Reply::error("ERR no such key")),
            Err(failed) => return Err(refused(&failed)),
        };
        let Value::Object(fields) = value_of(outcome) else {
            return Ok(Reply::Array(Vec::new()));
        };
        let last = fields.get("last").and_then(whole).unwrap_or(0);
        let names: Vec<String> = match fields.get("groups") {
            Some(Value::Object(groups)) => groups.keys().cloned().collect(),
            _ => Vec::new(),
        };
        let mut answered = Vec::with_capacity(names.len());
        for name in names {
            let described = self.describe(key, &name).await?;
            let delivered = match described.position {
                0 => Id::LEAST,
                at => self
                    .entries(key, at.saturating_sub(1), 1)
                    .await?
                    .pop()
                    .filter(|found: &Entry| found.position == at)
                    .map_or(Id { ms: 0, seq: at }, |found| found.id()),
            };
            answered.push(Reply::Map(vec![
                (Reply::text("name"), Reply::text(name.into_bytes())),
                (
                    Reply::text("consumers"),
                    count_reply(u64::try_from(described.members.len()).unwrap_or(u64::MAX)),
                ),
                (
                    Reply::text("pending"),
                    count_reply(u64::try_from(described.flight.len()).unwrap_or(u64::MAX)),
                ),
                (
                    Reply::text("last-delivered-id"),
                    Reply::text(delivered.text()),
                ),
                (Reply::text("entries-read"), count_reply(described.position)),
                (
                    Reply::text("lag"),
                    count_reply(last.saturating_sub(described.position)),
                ),
            ]));
        }
        Ok(Reply::Array(answered))
    }

    async fn members(&mut self, key: &str, group: &str) -> Result<Reply, Reply> {
        let described = self.describe(key, group).await?;
        Ok(Reply::Array(
            described
                .members
                .iter()
                .map(|member| {
                    Reply::Map(vec![
                        (Reply::text("name"), Reply::text(member.name.as_bytes())),
                        (Reply::text("pending"), count_reply(member.held)),
                        (Reply::text("idle"), since(member.seen)),
                        (Reply::text("inactive"), since(member.active)),
                    ])
                })
                .collect(),
        ))
    }
}
