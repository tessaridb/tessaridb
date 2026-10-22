//! Pub/sub, read as what it asks for (ADR-0133 D1): listening and leaving are
//! the connection's own, publishing and asking about channels go to the
//! node's registry once the store has said the caller may.

use crate::codec::Reply;
use crate::surface::channels::Kind;
use crate::surface::commands::{Procedure, Step};
use crate::surface::replies;

/// What `PUBSUB` asks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Asked {
    Channels(Kind, Option<Vec<u8>>),
    Listeners(Kind, Vec<Vec<u8>>),
    Patterns,
}

/// What a listening command names: channels, shard channels or patterns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Listening {
    Channels,
    Shards,
    Patterns,
}

impl Listening {
    /// The word its notices begin with, after `subscribe` or `unsubscribe`.
    pub(crate) const fn word(self, joining: bool) -> &'static str {
        match (self, joining) {
            (Self::Channels, true) => "subscribe",
            (Self::Channels, false) => "unsubscribe",
            (Self::Shards, true) => "ssubscribe",
            (Self::Shards, false) => "sunsubscribe",
            (Self::Patterns, true) => "psubscribe",
            (Self::Patterns, false) => "punsubscribe",
        }
    }
}

/// The commands a subscribed RESP2 connection still takes.
pub(crate) fn listening_command(name: &str) -> bool {
    matches!(
        name,
        "SUBSCRIBE"
            | "UNSUBSCRIBE"
            | "PSUBSCRIBE"
            | "PUNSUBSCRIBE"
            | "SSUBSCRIBE"
            | "SUNSUBSCRIBE"
            | "PING"
            | "QUIT"
            | "RESET"
    )
}

/// `name` as a step, or `None` when it is not pub/sub.
pub(crate) fn plan(name: &str, arguments: &[Vec<u8>]) -> Result<Option<Step>, Reply> {
    let procedure = match name {
        "SUBSCRIBE" | "PSUBSCRIBE" | "SSUBSCRIBE" => {
            if arguments.is_empty() {
                return Err(replies::arity(name));
            }
            Procedure::Listen {
                what: listening(name),
                names: arguments.to_vec(),
            }
        }
        "UNSUBSCRIBE" | "PUNSUBSCRIBE" | "SUNSUBSCRIBE" => Procedure::Leave {
            what: listening(name),
            names: arguments.to_vec(),
        },
        "PUBLISH" | "SPUBLISH" => match arguments {
            [channel, message] => Procedure::Publish {
                kind: if name == "SPUBLISH" {
                    Kind::Shard
                } else {
                    Kind::Plain
                },
                channel: channel.clone(),
                message: message.clone(),
            },
            _ => return Err(replies::arity(name)),
        },
        "PUBSUB" => return pubsub(arguments).map(Some),
        _ => return Ok(None),
    };
    Ok(Some(Step::Procedure(procedure)))
}

fn listening(name: &str) -> Listening {
    match name {
        "PSUBSCRIBE" | "PUNSUBSCRIBE" => Listening::Patterns,
        "SSUBSCRIBE" | "SUNSUBSCRIBE" => Listening::Shards,
        _ => Listening::Channels,
    }
}

fn pubsub(arguments: &[Vec<u8>]) -> Result<Step, Reply> {
    let Some((sub, rest)) = arguments.split_first() else {
        return Err(replies::arity("pubsub"));
    };
    let sub = String::from_utf8_lossy(sub).to_ascii_uppercase();
    let asked = match (sub.as_str(), rest) {
        ("HELP", []) => return Ok(Step::Reply(help())),
        ("CHANNELS", []) => Asked::Channels(Kind::Plain, None),
        ("CHANNELS", [pattern]) => Asked::Channels(Kind::Plain, Some(pattern.clone())),
        ("SHARDCHANNELS", []) => Asked::Channels(Kind::Shard, None),
        ("SHARDCHANNELS", [pattern]) => Asked::Channels(Kind::Shard, Some(pattern.clone())),
        ("NUMSUB", names) => Asked::Listeners(Kind::Plain, names.to_vec()),
        ("SHARDNUMSUB", names) => Asked::Listeners(Kind::Shard, names.to_vec()),
        ("NUMPAT", []) => Asked::Patterns,
        ("CHANNELS" | "SHARDCHANNELS" | "NUMPAT" | "HELP", _) => {
            return Err(replies::arity(&format!(
                "pubsub|{}",
                sub.to_ascii_lowercase()
            )));
        }
        _ => {
            return Err(Reply::error(format!(
                "ERR unknown subcommand '{}'. Try PUBSUB HELP.",
                sub.to_ascii_lowercase()
            )));
        }
    };
    Ok(Step::Procedure(Procedure::Pubsub(asked)))
}

fn help() -> Reply {
    Reply::Array(
        [
            "PUBSUB <subcommand> [<arg> [value] [opt] ...]. Subcommands are:",
            "CHANNELS [<pattern>]",
            "    Return the currently active channels matching a <pattern> (default: '*').",
            "NUMPAT",
            "    Return number of subscriptions to patterns.",
            "NUMSUB [<channel> ...]",
            "    Return the number of subscribers for the specified channels, excluding",
            "    pattern subscriptions(default: no channels).",
            "SHARDCHANNELS [<pattern>]",
            "    Return the currently active shard level channels matching a <pattern> (default: '*').",
            "SHARDNUMSUB [<shardchannel> ...]",
            "    Return the number of subscribers for the specified shard level channel(s)",
            "HELP",
            "    Print this help.",
        ]
        .into_iter()
        .map(Reply::text)
        .collect(),
    )
}
