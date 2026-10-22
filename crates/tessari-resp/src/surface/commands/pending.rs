//! What a stream group holds, asked and handed on — `XPENDING`, `XCLAIM`,
//! `XAUTOCLAIM`, `XINFO GROUPS | CONSUMERS` — read as what they ask of a
//! topic's group (ADR-0133).

use crate::codec::Reply;
use crate::surface::commands::arguments::integer;
use crate::surface::commands::streams::{Id, count, id, key, lower, upper};
use crate::surface::commands::{Procedure, Step};
use crate::surface::replies;

const NAMES: &[&str] = &["XPENDING", "XCLAIM", "XAUTOCLAIM"];

/// Whether `name` is one of these commands.
pub(super) fn serves(name: &str) -> bool {
    NAMES.contains(&name)
}

/// A question about what a group holds, or a hand-off of it.
pub(crate) enum Pending {
    /// `XPENDING key group`.
    Summary { key: String, group: String },
    /// `XPENDING key group [IDLE ms] start end count [consumer]`.
    Listed {
        key: String,
        group: String,
        idle: u64,
        from: Id,
        to: Id,
        count: u64,
        consumer: Option<String>,
    },
    /// `XCLAIM` and `XAUTOCLAIM`: held entries handed to `consumer`.
    Claim {
        key: String,
        group: String,
        consumer: String,
        /// The least a holder must have been idle, in milliseconds.
        idle: u64,
        which: Claimed,
        /// `JUSTID`: answer the IDs alone.
        ids: bool,
    },
    /// `XINFO GROUPS key`.
    Groups(String),
    /// `XINFO CONSUMERS key group`.
    Members { key: String, group: String },
}

/// Which held entries a claim reaches.
pub(crate) enum Claimed {
    /// These positions.
    At(Vec<u64>),
    /// Up to `count` after this position, answering where the next scan starts.
    After { after: u64, count: u64 },
}

/// A group or member name, which is text.
fn name(bytes: &[u8]) -> Result<String, Reply> {
    String::from_utf8(bytes.to_vec())
        .map_err(|_| Reply::error("ERR a group or consumer name is UTF-8 text on this node"))
}

/// An ID's position: its second half.
fn position(bytes: &[u8]) -> Result<u64, Reply> {
    id(bytes).map(|(_, seq)| seq.unwrap_or(0))
}

fn xpending(arguments: &[Vec<u8>]) -> Result<Pending, Reply> {
    let [stream, group, rest @ ..] = arguments else {
        return Err(replies::arity("XPENDING"));
    };
    let (key, group) = (key(stream)?, name(group)?);
    let (idle, rest) = match rest {
        [word, ms, rest @ ..] if word.eq_ignore_ascii_case(b"IDLE") => (count(ms)?, rest),
        rest => (0, rest),
    };
    Ok(match rest {
        [] if idle == 0 => Pending::Summary { key, group },
        [start, end, n, consumer @ ..] if consumer.len() <= 1 => Pending::Listed {
            key,
            group,
            idle,
            from: lower(start)?.unwrap_or(Id::MOST),
            to: upper(end)?.unwrap_or(Id::LEAST),
            count: u64::try_from(integer(n)?).unwrap_or(0),
            consumer: consumer.first().map(|bytes| name(bytes)).transpose()?,
        },
        _ => return Err(replies::syntax()),
    })
}

fn claim(name_of: &str, arguments: &[Vec<u8>]) -> Result<Pending, Reply> {
    let [stream, group, consumer, idle, rest @ ..] = arguments else {
        return Err(replies::arity(name_of));
    };
    let (key, group, consumer) = (key(stream)?, name(group)?, name(consumer)?);
    let idle = count(idle)?;
    let mut ids = false;
    let which = if name_of == "XCLAIM" {
        let split = rest
            .iter()
            .position(|word| id(word).is_err())
            .unwrap_or(rest.len());
        let (given, options) = rest.split_at(split);
        if given.is_empty() {
            return Err(replies::arity(name_of));
        }
        // `IDLE`, `TIME`, `RETRYCOUNT`, `FORCE` and `LASTID` set what the
        // engine keeps for itself; they are read and have no effect here.
        let mut options = options;
        while let [word, more @ ..] = options {
            let word = word.to_ascii_uppercase();
            options = match (word.as_slice(), more) {
                (b"JUSTID", more) => {
                    ids = true;
                    more
                }
                (b"FORCE", more) => more,
                (b"IDLE" | b"TIME" | b"RETRYCOUNT" | b"LASTID", [_, more @ ..]) => more,
                _ => return Err(replies::syntax()),
            };
        }
        Claimed::At(
            given
                .iter()
                .map(|given| position(given))
                .collect::<Result<_, _>>()?,
        )
    } else {
        let [start, options @ ..] = rest else {
            return Err(replies::arity(name_of));
        };
        let mut wanted = 100;
        let mut options = options;
        while let [word, more @ ..] = options {
            options = match (word.to_ascii_uppercase().as_slice(), more) {
                (b"COUNT", [n, more @ ..]) => {
                    wanted = count(n)?.max(1);
                    more
                }
                (b"JUSTID", more) => {
                    ids = true;
                    more
                }
                _ => return Err(replies::syntax()),
            };
        }
        Claimed::After {
            after: position(start)?.saturating_sub(1),
            count: wanted,
        }
    };
    Ok(Pending::Claim {
        key,
        group,
        consumer,
        idle,
        which,
        ids,
    })
}

/// `XINFO GROUPS key` and `XINFO CONSUMERS key group`.
pub(super) fn info(arguments: &[Vec<u8>]) -> Result<Option<Step>, Reply> {
    let pending = match arguments {
        [what, stream] if what.eq_ignore_ascii_case(b"GROUPS") => Pending::Groups(key(stream)?),
        [what, stream, group] if what.eq_ignore_ascii_case(b"CONSUMERS") => Pending::Members {
            key: key(stream)?,
            group: name(group)?,
        },
        _ => return Err(replies::syntax()),
    };
    Ok(Some(Step::Procedure(Procedure::Pending(pending))))
}

/// Plan `name` when it is one of these commands.
pub(super) fn plan(name: &str, arguments: &[Vec<u8>]) -> Result<Option<Step>, Reply> {
    if !serves(name) {
        return Ok(None);
    }
    let pending = match name {
        "XPENDING" => xpending(arguments)?,
        _ => claim(name, arguments)?,
    };
    Ok(Some(Step::Procedure(Procedure::Pending(pending))))
}
