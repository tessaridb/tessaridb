//! The stream commands — `XADD`, `XLEN`, `XTRIM`, `XRANGE`, `XREVRANGE`,
//! `XREAD`, `XINFO STREAM` — read as what they ask of a topic (ADR-0133).
//!
//! A stream key is a TOPIC NAME in the connection's database, and a name is
//! grammar: it is checked as an identifier and never bound. An entry's ID is
//! `<appended ms>-<position>`, decided by the commit that appends it, so an
//! `XADD` naming its own ID is refused rather than renumbered.

use std::time::Duration;

use crate::codec::Reply;
use crate::surface::commands::arguments::integer;
use crate::surface::commands::{Procedure, Step};
use crate::surface::replies;
use crate::surface::space::is_identifier;

const NAMES: &[&str] = &[
    "XADD",
    "XLEN",
    "XTRIM",
    "XRANGE",
    "XREVRANGE",
    "XREAD",
    "XINFO",
    "XDEL",
    "XSETID",
];

/// Whether `name` is one of these commands.
pub(super) fn serves(name: &str) -> bool {
    NAMES.contains(&name)
}

/// An entry's ID, ordered by its millisecond and then its position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Id {
    pub(crate) ms: u64,
    pub(crate) seq: u64,
}

impl Id {
    /// Below every ID.
    pub(crate) const LEAST: Self = Self { ms: 0, seq: 0 };
    /// Above every ID.
    pub(crate) const MOST: Self = Self {
        ms: u64::MAX,
        seq: u64::MAX,
    };

    /// The next ID up, or `None` above the last.
    pub(crate) const fn next(self) -> Option<Self> {
        match self.seq.checked_add(1) {
            Some(seq) => Some(Self { ms: self.ms, seq }),
            None => match self.ms.checked_add(1) {
                Some(ms) => Some(Self { ms, seq: 0 }),
                None => None,
            },
        }
    }

    /// The next ID down, or `None` below the first.
    const fn previous(self) -> Option<Self> {
        match self.seq.checked_sub(1) {
            Some(seq) => Some(Self { ms: self.ms, seq }),
            None => match self.ms.checked_sub(1) {
                Some(ms) => Some(Self { ms, seq: u64::MAX }),
                None => None,
            },
        }
    }

    /// As a client is given it.
    pub(crate) fn text(self) -> String {
        format!("{}-{}", self.ms, self.seq)
    }
}

/// How much of the front of a stream goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Trim {
    /// `MAXLEN n`: all but the newest `n`.
    Keep(u64),
    /// `MINID id`: every entry below `id`.
    Below(Id),
}

/// Where an `XREAD` of one stream starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Since {
    /// Past this ID.
    After(Id),
    /// `$`: past the last entry when the command arrived.
    Newest,
    /// `+`: the last entry itself.
    Last,
}

/// A stream command.
pub(crate) enum Stream {
    Add {
        key: String,
        /// Create the topic when it is not there (no `NOMKSTREAM`).
        make: bool,
        trim: Option<Trim>,
        fields: Vec<(String, Vec<u8>)>,
    },
    Len(String),
    Trim {
        key: String,
        trim: Trim,
    },
    /// The entries from `from` to `to`, both IDs included.
    Range {
        key: String,
        from: Id,
        to: Id,
        count: Option<u64>,
        reverse: bool,
    },
    Read {
        count: Option<u64>,
        keys: Vec<(String, Since)>,
        /// `BLOCK ms`; `Some(None)` waits with no limit.
        block: Option<Option<Duration>>,
    },
    Info(String),
}

/// A key as the topic it names.
pub(super) fn key(bytes: &[u8]) -> Result<String, Reply> {
    std::str::from_utf8(bytes)
        .ok()
        .filter(|name| is_identifier(name))
        .map(str::to_owned)
        .ok_or_else(|| {
            Reply::error(
                "ERR a stream key is a topic name on this node: letters, digits and \
                 underscores, not starting with a digit",
            )
        })
}

fn invalid_id() -> Reply {
    Reply::error("ERR Invalid stream ID specified as stream command argument")
}

/// `ms` or `ms-seq`; `ms` alone answers `None` for its sequence.
pub(super) fn id(bytes: &[u8]) -> Result<(u64, Option<u64>), Reply> {
    let text = std::str::from_utf8(bytes).map_err(|_| invalid_id())?;
    let number = |part: &str| {
        part.bytes()
            .all(|byte| byte.is_ascii_digit())
            .then(|| part.parse::<u64>().ok())
            .flatten()
            .ok_or_else(invalid_id)
    };
    match text.split_once('-') {
        Some((ms, seq)) => Ok((number(ms)?, Some(number(seq)?))),
        None => Ok((number(text)?, None)),
    }
}

/// The first ID a range starting at `bytes` includes.
pub(super) fn lower(bytes: &[u8]) -> Result<Option<Id>, Reply> {
    match bytes {
        b"-" => return Ok(Some(Id::LEAST)),
        b"+" => return Ok(Some(Id::MOST)),
        _ => {}
    }
    let (exclusive, bytes) = match bytes.strip_prefix(b"(") {
        Some(rest) => (true, rest),
        None => (false, bytes),
    };
    let (ms, seq) = id(bytes)?;
    let at = Id {
        ms,
        seq: seq.unwrap_or(0),
    };
    Ok(if exclusive { at.next() } else { Some(at) })
}

/// The last ID a range ending at `bytes` includes.
pub(super) fn upper(bytes: &[u8]) -> Result<Option<Id>, Reply> {
    match bytes {
        b"-" => return Ok(Some(Id::LEAST)),
        b"+" => return Ok(Some(Id::MOST)),
        _ => {}
    }
    let (exclusive, bytes) = match bytes.strip_prefix(b"(") {
        Some(rest) => (true, rest),
        None => (false, bytes),
    };
    let (ms, seq) = id(bytes)?;
    let at = Id {
        ms,
        seq: seq.unwrap_or(u64::MAX),
    };
    Ok(if exclusive { at.previous() } else { Some(at) })
}

/// A count that may not be negative.
pub(super) fn count(bytes: &[u8]) -> Result<u64, Reply> {
    u64::try_from(integer(bytes)?).map_err(|_| replies::not_an_integer())
}

/// A trim read off the front of some words, and the words after it.
type Trimmed<'a> = Option<(Trim, &'a [Vec<u8>])>;

/// `MAXLEN|MINID [=|~] threshold [LIMIT n]` at the start of `words`, and what
/// follows it; `None` when `words` does not start with one.
fn trim(words: &[Vec<u8>]) -> Result<Trimmed<'_>, Reply> {
    let [strategy, rest @ ..] = words else {
        return Ok(None);
    };
    let strategy = strategy.to_ascii_uppercase();
    if strategy != b"MAXLEN" && strategy != b"MINID" {
        return Ok(None);
    }
    let rest = match rest {
        [mark, rest @ ..] if mark == b"=" || mark == b"~" => rest,
        rest => rest,
    };
    let [threshold, rest @ ..] = rest else {
        return Err(replies::syntax());
    };
    let trim = if strategy == b"MAXLEN" {
        Trim::Keep(count(threshold)?)
    } else {
        let (ms, seq) = id(threshold)?;
        Trim::Below(Id {
            ms,
            seq: seq.unwrap_or(0),
        })
    };
    // `LIMIT` bounds how much an approximate trim does at once; a trim here is
    // one statement and always exact, so it is read and has nothing to bound.
    let rest = match rest {
        [word, limit, rest @ ..] if word.eq_ignore_ascii_case(b"LIMIT") => {
            count(limit)?;
            rest
        }
        rest => rest,
    };
    Ok(Some((trim, rest)))
}

fn add(arguments: &[Vec<u8>]) -> Result<Stream, Reply> {
    let [name, rest @ ..] = arguments else {
        return Err(replies::arity("XADD"));
    };
    let key = key(name)?;
    let mut make = true;
    let mut cut = None;
    let mut rest = rest;
    loop {
        if let [word, more @ ..] = rest
            && word.eq_ignore_ascii_case(b"NOMKSTREAM")
        {
            make = false;
            rest = more;
            continue;
        }
        if let Some((found, more)) = trim(rest)? {
            cut = Some(found);
            rest = more;
            continue;
        }
        break;
    }
    let [given, pairs @ ..] = rest else {
        return Err(replies::arity("XADD"));
    };
    if given.as_slice() != b"*" {
        id(given)?;
        return Err(Reply::error(
            "ERR an entry's ID is decided when it commits on this node; send * instead",
        ));
    }
    if pairs.is_empty() || pairs.len() % 2 != 0 {
        return Err(replies::arity("XADD"));
    }
    let mut fields: Vec<(String, Vec<u8>)> = Vec::with_capacity(pairs.len() / 2);
    let (pairs, _) = pairs.as_chunks::<2>();
    for [field, value] in pairs {
        let field = String::from_utf8(field.clone())
            .map_err(|_| Reply::error("ERR a field name is UTF-8 text on this node"))?;
        if fields.iter().any(|(known, _)| *known == field) {
            return Err(Reply::error(
                "ERR an entry holds each field once on this node",
            ));
        }
        fields.push((field, value.clone()));
    }
    Ok(Stream::Add {
        key,
        make,
        trim: cut,
        fields,
    })
}

fn range(name: &str, arguments: &[Vec<u8>]) -> Result<Stream, Reply> {
    let [key_bytes, first, second, rest @ ..] = arguments else {
        return Err(replies::arity(name));
    };
    let reverse = name == "XREVRANGE";
    let (start, end) = if reverse {
        (second, first)
    } else {
        (first, second)
    };
    let count = match rest {
        [] => None,
        [word, n] if word.eq_ignore_ascii_case(b"COUNT") => {
            Some(u64::try_from(integer(n)?).unwrap_or(0))
        }
        _ => return Err(replies::syntax()),
    };
    let key = key(key_bytes)?;
    // A bound past the end of the IDs is a range holding nothing.
    match (lower(start)?, upper(end)?) {
        (Some(from), Some(to)) => Ok(Stream::Range {
            key,
            from,
            to,
            count,
            reverse,
        }),
        _ => Ok(Stream::Range {
            key,
            from: Id::MOST,
            to: Id::LEAST,
            count,
            reverse,
        }),
    }
}

fn read(arguments: &[Vec<u8>]) -> Result<Stream, Reply> {
    let mut count = None;
    let mut block = None;
    let mut rest = arguments;
    let streams = loop {
        match rest {
            [word, n, more @ ..] if word.eq_ignore_ascii_case(b"COUNT") => {
                count = Some(u64::try_from(integer(n)?).unwrap_or(0));
                rest = more;
            }
            [word, ms, more @ ..] if word.eq_ignore_ascii_case(b"BLOCK") => {
                let ms = integer(ms)?;
                let ms = u64::try_from(ms).map_err(|_| Reply::error("ERR timeout is negative"))?;
                block = Some((ms > 0).then(|| Duration::from_millis(ms)));
                rest = more;
            }
            [word, more @ ..] if word.eq_ignore_ascii_case(b"STREAMS") => break more,
            _ => return Err(replies::syntax()),
        }
    };
    if streams.is_empty() || streams.len() % 2 != 0 {
        return Err(Reply::error(
            "ERR Unbalanced 'xread' list of streams: for each stream key an ID or '$' \
             must be specified.",
        ));
    }
    let (names, ids) = streams.split_at(streams.len() / 2);
    let mut keys = Vec::with_capacity(names.len());
    for (name, given) in names.iter().zip(ids) {
        let since = match given.as_slice() {
            b"$" => Since::Newest,
            b"+" => Since::Last,
            given => {
                let (ms, seq) = id(given)?;
                Since::After(Id {
                    ms,
                    seq: seq.unwrap_or(0),
                })
            }
        };
        keys.push((key(name)?, since));
    }
    Ok(Stream::Read { count, keys, block })
}

/// Plan `name` when it is a stream command.
pub(super) fn plan(name: &str, arguments: &[Vec<u8>]) -> Result<Option<Step>, Reply> {
    if !serves(name) {
        return Ok(None);
    }
    let stream = match name {
        "XADD" => add(arguments)?,
        "XLEN" => match arguments {
            [name] => Stream::Len(key(name)?),
            _ => return Err(replies::arity(name)),
        },
        "XTRIM" => {
            let [name, rest @ ..] = arguments else {
                return Err(replies::arity("XTRIM"));
            };
            match trim(rest)? {
                Some((trim, [])) => Stream::Trim {
                    key: key(name)?,
                    trim,
                },
                _ => return Err(replies::syntax()),
            }
        }
        "XRANGE" | "XREVRANGE" => range(name, arguments)?,
        "XREAD" => read(arguments)?,
        "XINFO" => match arguments {
            [what, name] if what.eq_ignore_ascii_case(b"STREAM") => Stream::Info(key(name)?),
            [what, ..] if what.eq_ignore_ascii_case(b"STREAM") => {
                return Err(Reply::error(
                    "ERR XINFO STREAM FULL is not offered on this node",
                ));
            }
            _ => return super::pending::info(arguments),
        },
        // A message is never changed and its position never chosen.
        "XDEL" => {
            return Err(Reply::error(
                "ERR a topic is append-only: an entry is removed only from the front, by \
                 XTRIM",
            ));
        }
        _ => {
            return Err(Reply::error(
                "ERR an entry's ID is decided when it commits on this node; XSETID is not \
                 offered",
            ));
        }
    };
    Ok(Some(Step::Procedure(Procedure::Stream(stream))))
}
