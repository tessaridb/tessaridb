//! The stream group commands — `XGROUP`, `XREADGROUP`, `XACK` — read as what
//! they ask of a topic's group (ADR-0133).
//!
//! A stream group is a topic group declared `ACK DEADLINE NONE`: an entry
//! handed to a member stays held until it is acknowledged or handed on, which
//! is the protocol's pending list. Group and member names are values, bound;
//! the stream key is a topic name, checked.

use std::time::Duration;

use crate::codec::Reply;
use crate::surface::commands::arguments::integer;
use crate::surface::commands::streams::{Id, count, id, key};
use crate::surface::commands::{Procedure, Step};
use crate::surface::replies;

const NAMES: &[&str] = &["XGROUP", "XREADGROUP", "XACK"];

/// Whether `name` is one of these commands.
pub(super) fn serves(name: &str) -> bool {
    NAMES.contains(&name)
}

/// Where a group starts handing out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Start {
    /// `$`: past the last entry.
    Newest,
    /// Past this ID.
    After(Id),
}

/// What one stream of an `XREADGROUP` asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Wanted {
    /// `>`: entries never handed out.
    New,
    /// The member's own held entries past this position.
    Held(u64),
}

/// A stream group command.
pub(crate) enum Group {
    Create {
        key: String,
        group: String,
        start: Start,
        /// `MKSTREAM`: declare the topic when it is not there.
        make: bool,
    },
    SetId {
        key: String,
        group: String,
        start: Start,
    },
    Destroy {
        key: String,
        group: String,
    },
    CreateConsumer {
        key: String,
        group: String,
        consumer: String,
    },
    DelConsumer {
        key: String,
        group: String,
        consumer: String,
    },
    Read {
        group: String,
        consumer: String,
        count: Option<u64>,
        /// `BLOCK ms`; `Some(None)` waits with no limit.
        block: Option<Option<Duration>>,
        /// `NOACK`: acknowledged as soon as it is handed out.
        settle: bool,
        keys: Vec<(String, Wanted)>,
    },
    Ack {
        key: String,
        group: String,
        positions: Vec<u64>,
    },
}

/// A group or member name, which is text.
fn name(bytes: &[u8]) -> Result<String, Reply> {
    String::from_utf8(bytes.to_vec())
        .map_err(|_| Reply::error("ERR a group or consumer name is UTF-8 text on this node"))
}

fn start(bytes: &[u8]) -> Result<Start, Reply> {
    if bytes == b"$" {
        return Ok(Start::Newest);
    }
    let (ms, seq) = id(bytes)?;
    Ok(Start::After(Id {
        ms,
        seq: seq.unwrap_or(0),
    }))
}

fn xgroup(arguments: &[Vec<u8>]) -> Result<Group, Reply> {
    let [subcommand, rest @ ..] = arguments else {
        return Err(replies::arity("XGROUP"));
    };
    let subcommand = subcommand.to_ascii_uppercase();
    Ok(match (subcommand.as_slice(), rest) {
        (b"CREATE", [stream, group, from, options @ ..]) => {
            let mut make = false;
            let mut options = options;
            while let [word, more @ ..] = options {
                if word.eq_ignore_ascii_case(b"MKSTREAM") {
                    make = true;
                    options = more;
                } else if word.eq_ignore_ascii_case(b"ENTRIESREAD") {
                    // How many entries a group has read is its position here.
                    let [_, n, more @ ..] = options else {
                        return Err(replies::syntax());
                    };
                    count(n)?;
                    options = more;
                } else {
                    return Err(replies::syntax());
                }
            }
            Group::Create {
                key: key(stream)?,
                group: name(group)?,
                start: start(from)?,
                make,
            }
        }
        (b"SETID", [stream, group, from, ..]) => Group::SetId {
            key: key(stream)?,
            group: name(group)?,
            start: start(from)?,
        },
        (b"DESTROY", [stream, group]) => Group::Destroy {
            key: key(stream)?,
            group: name(group)?,
        },
        (b"CREATECONSUMER", [stream, group, consumer]) => Group::CreateConsumer {
            key: key(stream)?,
            group: name(group)?,
            consumer: name(consumer)?,
        },
        (b"DELCONSUMER", [stream, group, consumer]) => Group::DelConsumer {
            key: key(stream)?,
            group: name(group)?,
            consumer: name(consumer)?,
        },
        (b"CREATE" | b"SETID" | b"DESTROY" | b"CREATECONSUMER" | b"DELCONSUMER", _) => {
            return Err(replies::arity("XGROUP"));
        }
        _ => return Err(replies::syntax()),
    })
}

fn xreadgroup(arguments: &[Vec<u8>]) -> Result<Group, Reply> {
    let [word, group, consumer, rest @ ..] = arguments else {
        return Err(replies::arity("XREADGROUP"));
    };
    if !word.eq_ignore_ascii_case(b"GROUP") {
        return Err(replies::syntax());
    }
    let mut count = None;
    let mut block = None;
    let mut settle = false;
    let mut rest = rest;
    let streams = loop {
        match rest {
            [word, n, more @ ..] if word.eq_ignore_ascii_case(b"COUNT") => {
                count = Some(u64::try_from(integer(n)?).unwrap_or(0));
                rest = more;
            }
            [word, ms, more @ ..] if word.eq_ignore_ascii_case(b"BLOCK") => {
                let ms = u64::try_from(integer(ms)?)
                    .map_err(|_| Reply::error("ERR timeout is negative"))?;
                block = Some((ms > 0).then(|| Duration::from_millis(ms)));
                rest = more;
            }
            [word, more @ ..] if word.eq_ignore_ascii_case(b"NOACK") => {
                settle = true;
                rest = more;
            }
            [word, more @ ..] if word.eq_ignore_ascii_case(b"STREAMS") => break more,
            _ => return Err(replies::syntax()),
        }
    };
    if streams.is_empty() || streams.len() % 2 != 0 {
        return Err(Reply::error(
            "ERR Unbalanced 'xreadgroup' list of streams: for each stream key an ID or '>' \
             must be specified.",
        ));
    }
    let (names, ids) = streams.split_at(streams.len() / 2);
    let mut keys = Vec::with_capacity(names.len());
    for (stream, given) in names.iter().zip(ids) {
        let wanted = if given.as_slice() == b">" {
            Wanted::New
        } else {
            // A held entry's position is its ID's second half.
            let (_, seq) = id(given)?;
            Wanted::Held(seq.unwrap_or(0))
        };
        keys.push((key(stream)?, wanted));
    }
    Ok(Group::Read {
        group: name(group)?,
        consumer: name(consumer)?,
        count,
        block,
        settle,
        keys,
    })
}

fn xack(arguments: &[Vec<u8>]) -> Result<Group, Reply> {
    let [stream, group, ids @ ..] = arguments else {
        return Err(replies::arity("XACK"));
    };
    if ids.is_empty() {
        return Err(replies::arity("XACK"));
    }
    let mut positions = Vec::with_capacity(ids.len());
    for given in ids {
        let (_, seq) = id(given)?;
        positions.push(seq.unwrap_or(0));
    }
    Ok(Group::Ack {
        key: key(stream)?,
        group: name(group)?,
        positions,
    })
}

/// Plan `name` when it is a stream group command.
pub(super) fn plan(name: &str, arguments: &[Vec<u8>]) -> Result<Option<Step>, Reply> {
    if !serves(name) {
        return Ok(None);
    }
    let group = match name {
        "XGROUP" => xgroup(arguments)?,
        "XREADGROUP" => xreadgroup(arguments)?,
        _ => xack(arguments)?,
    };
    Ok(Some(Step::Procedure(Procedure::Group(group))))
}
