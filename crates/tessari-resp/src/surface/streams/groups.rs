//! The stream group commands carried out against a topic's group.
//!
//! A RESP group is declared `ACK DEADLINE NONE` so an entry stays held until
//! it is acknowledged, and `IN FLIGHT` at the engine's ceiling so a group
//! holds as many as it can; a read finding the flight full says so rather
//! than answering an empty read that looks like an idle stream.

use tessaridb::{Parameters, Value};

use super::entry::{self, Entry, PAGE, integer, missing, refused, whole};
use crate::codec::Reply;
use crate::surface::commands::{Group, Id, Start, Wanted};
use crate::surface::procedures::Against;
use crate::surface::running::Failed;
use crate::surface::statement::value_of;

/// How many entries a group holds unacknowledged: the engine's ceiling.
const WIDTH: u64 = 10_000;

fn no_key() -> Reply {
    Reply::error(
        "ERR The XGROUP subcommand requires the key to exist. Note that for CREATE you may \
         want to use the MKSTREAM option to create an empty stream automatically.",
    )
}

pub(super) fn no_group(key: &str, group: &str) -> Reply {
    Reply::error(format!(
        "NOGROUP No such key '{key}' or consumer group '{group}'"
    ))
}

/// Whether `failed` says the group is not there: the topic has no group of
/// that name, or the name reads it as a plain consumer.
pub(super) const fn not_a_group(failed: &Failed) -> bool {
    matches!(
        failed,
        Failed::Refused(
            tessaridb::Error::NoSuchGroup { .. }
                | tessaridb::Error::NotAGroup { .. }
                | tessaridb::Error::NoGroupForMember { .. }
        )
    )
}

/// A group's and a member's names, bound.
fn named(group: &str, member: Option<&str>) -> Parameters {
    let mut parameters = Parameters::new();
    parameters.insert("group".to_owned(), Value::from(group));
    if let Some(member) = member {
        parameters.insert("member".to_owned(), Value::from(member));
    }
    parameters
}

impl Against<'_> {
    /// Carry out a stream group command, `XREADGROUP … BLOCK` as its one try.
    pub(crate) async fn group(&mut self, group: &Group) -> Reply {
        let answered = match group {
            Group::Create {
                key,
                group,
                start,
                make,
            } => self.create(key, group, *start, *make).await,
            Group::SetId { key, group, start } => self.set_id(key, group, *start).await,
            Group::Destroy { key, group } => self.destroy(key, group).await,
            Group::CreateConsumer {
                key,
                group,
                consumer,
            } => {
                self.member(key, group, consumer, "ADD MEMBER $member")
                    .await
            }
            Group::DelConsumer {
                key,
                group,
                consumer,
            } => {
                self.member(key, group, consumer, "DROP MEMBER $member")
                    .await
            }
            Group::Read {
                group,
                consumer,
                count,
                settle,
                keys,
                ..
            } => {
                self.read_group((group, consumer), *count, *settle, keys)
                    .await
            }
            Group::Ack {
                key,
                group,
                positions,
            } => self.ack(key, group, positions).await,
        };
        answered.unwrap_or_else(|reply| reply)
    }

    /// The position a group starting at `start` stands at: it hands out the
    /// next one.
    async fn standing(&mut self, key: &str, start: Start) -> Result<u64, Reply> {
        let held = self.held(key).await?.ok_or_else(no_key)?;
        match start {
            Start::Newest => Ok(held.last),
            Start::After(id) if id == Id::LEAST => Ok(0),
            Start::After(id) => match id.next() {
                Some(next) => Ok(self.first_at(key, &held, next).await?.saturating_sub(1)),
                None => Ok(held.last),
            },
        }
    }

    async fn create(
        &mut self,
        key: &str,
        group: &str,
        start: Start,
        make: bool,
    ) -> Result<Reply, Reply> {
        if make && self.held(key).await?.is_none() {
            self.one(
                format!("DEFINE TOPIC IF NOT EXISTS {key}"),
                Parameters::new(),
            )
            .await
            .map_err(|failed| refused(&failed))?;
        }
        let at = self.standing(key, start).await?;
        let mut parameters = named(group, None);
        parameters.insert("at".to_owned(), integer(at));
        let statements = [
            format!("DEFINE GROUP $group ON TOPIC {key} ACK DEADLINE NONE IN FLIGHT {WIDTH}"),
            format!("ALTER GROUP $group ON TOPIC {key} START AT $at"),
        ];
        match self.run(&statements, &parameters).await {
            Ok(_) => Ok(Reply::OK),
            Err(Failed::Refused(tessaridb::Error::GroupExists { .. })) => {
                Err(Reply::error("BUSYGROUP Consumer Group name already exists"))
            }
            Err(failed) => Err(refused(&failed)),
        }
    }

    async fn set_id(&mut self, key: &str, group: &str, start: Start) -> Result<Reply, Reply> {
        let at = self.standing(key, start).await?;
        let mut parameters = named(group, None);
        parameters.insert("at".to_owned(), integer(at));
        match self
            .one(
                format!("ALTER GROUP $group ON TOPIC {key} START AT $at"),
                parameters,
            )
            .await
        {
            Ok(_) => Ok(Reply::OK),
            Err(failed) if not_a_group(&failed) => Err(no_group(key, group)),
            Err(failed) => Err(refused(&failed)),
        }
    }

    async fn destroy(&mut self, key: &str, group: &str) -> Result<Reply, Reply> {
        if self.held(key).await?.is_none() {
            return Err(no_key());
        }
        match self
            .one(
                format!("INFO FOR GROUP $group ON TOPIC {key}"),
                named(group, None),
            )
            .await
        {
            Ok(_) => {}
            Err(failed) if not_a_group(&failed) => return Ok(Reply::Integer(0)),
            Err(failed) => return Err(refused(&failed)),
        }
        self.one(
            format!("DROP GROUP $group ON TOPIC {key}"),
            named(group, None),
        )
        .await
        .map_err(|failed| refused(&failed))?;
        Ok(Reply::Integer(1))
    }

    /// `ALTER GROUP … ADD MEMBER | DROP MEMBER`, answering the count it gives.
    async fn member(
        &mut self,
        key: &str,
        group: &str,
        consumer: &str,
        change: &str,
    ) -> Result<Reply, Reply> {
        match self
            .one(
                format!("ALTER GROUP $group ON TOPIC {key} {change}"),
                named(group, Some(consumer)),
            )
            .await
        {
            Ok(outcome) => Ok(Reply::Integer(
                whole(&value_of(outcome))
                    .and_then(|count| i64::try_from(count).ok())
                    .unwrap_or(0),
            )),
            Err(failed) if missing(&failed) => Err(no_key()),
            Err(failed) if not_a_group(&failed) => Err(no_group(key, group)),
            Err(failed) => Err(refused(&failed)),
        }
    }

    async fn read_group(
        &mut self,
        (group, consumer): (&str, &str),
        count: Option<u64>,
        settle: bool,
        keys: &[(String, Wanted)],
    ) -> Result<Reply, Reply> {
        let limit = count.unwrap_or(PAGE).clamp(1, WIDTH);
        let mut answered = Vec::new();
        for (key, wanted) in keys {
            let mut parameters = named(group, Some(consumer));
            parameters.insert("limit".to_owned(), integer(limit));
            let statement = match wanted {
                Wanted::New => {
                    format!("READ FROM {key} FOR CONSUMER $group MEMBER $member LIMIT $limit")
                }
                Wanted::Held(after) => {
                    parameters.insert("after".to_owned(), integer(*after));
                    format!(
                        "READ FROM {key} FOR CONSUMER $group MEMBER $member HELD AFTER $after \
                         LIMIT $limit"
                    )
                }
            };
            let entries = match self.one(statement, parameters).await {
                Ok(outcome) => entry::read(outcome),
                Err(failed) if missing(&failed) || not_a_group(&failed) => {
                    return Err(no_group(key, group));
                }
                Err(failed) => return Err(refused(&failed)),
            };
            if *wanted == Wanted::New {
                if entries.is_empty() {
                    self.full(key, group).await?;
                    continue;
                }
                if settle {
                    self.ack(key, group, &positions(&entries)).await?;
                }
            }
            answered.push((
                Reply::text(key.as_bytes()),
                Reply::Array(entries.iter().map(Entry::reply).collect()),
            ));
        }
        Ok(if answered.is_empty() {
            Reply::NilArray
        } else {
            Reply::Keyed(answered)
        })
    }

    /// An error when the group's flight is full, which is why a read handed
    /// out nothing.
    async fn full(&mut self, key: &str, group: &str) -> Result<(), Reply> {
        let outcome = self
            .one(
                format!("INFO FOR GROUP $group ON TOPIC {key}"),
                named(group, None),
            )
            .await
            .map_err(|failed| refused(&failed))?;
        let Value::Object(fields) = value_of(outcome) else {
            return Ok(());
        };
        let held = fields.get("in_flight").and_then(whole).unwrap_or(0);
        let width = fields.get("width").and_then(whole).unwrap_or(WIDTH);
        if held >= width {
            return Err(Reply::error(format!(
                "ERR group '{group}' holds {held} entries unacknowledged, the most it holds; \
                 XACK some before reading more"
            )));
        }
        Ok(())
    }

    async fn ack(&mut self, key: &str, group: &str, positions: &[u64]) -> Result<Reply, Reply> {
        let mut parameters = named(group, None);
        parameters.insert(
            "positions".to_owned(),
            Value::Array(positions.iter().map(|at| integer(*at)).collect()),
        );
        match self
            .one(
                format!("ACK {key} FOR CONSUMER $group AT $positions"),
                parameters,
            )
            .await
        {
            Ok(outcome) => Ok(Reply::Integer(
                whole(&value_of(outcome))
                    .and_then(|count| i64::try_from(count).ok())
                    .unwrap_or(0),
            )),
            Err(failed) if missing(&failed) || not_a_group(&failed) => Ok(Reply::Integer(0)),
            Err(failed) => Err(refused(&failed)),
        }
    }
}

fn positions(entries: &[Entry]) -> Vec<u64> {
    entries.iter().map(|entry| entry.position).collect()
}
