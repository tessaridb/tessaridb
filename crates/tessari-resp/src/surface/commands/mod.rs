//! A command, read as what it asks for.
//!
//! Most commands are statements and are planned here as text plus a decoder
//! (see `statement`). The rest take more than one round trip, or act on the
//! connection rather than the store, and are named as a [`Step`] the
//! connection carries out.

mod arguments;
mod blocking;
mod change;
mod combine;
mod containers;
mod fields;
mod groups;
mod hashes;
mod keys;
mod lists;
mod pops;
mod pubsub;
mod ranges;
mod scans;
mod sets;
mod streams;
mod strings;
mod zsets;

use crate::codec::{Command, Reply};
use crate::surface::Space;
use crate::surface::replies;
use crate::surface::statement::{Binder, Planned};

pub(crate) use arguments::{Expiry, now_millis};
pub(crate) use blocking::{Shape, found_nothing};
pub(crate) use change::{Change, changed};
pub(crate) use containers::kind;
pub(crate) use groups::{Group, Start, Wanted};
pub(crate) use keys::{When, expire};
pub(crate) use pubsub::{Asked, Listening, listening_command};
pub(crate) use streams::{Id, Since, Stream, Trim};
pub(crate) use strings::string_reply;

/// What a command asks for.
pub(crate) enum Step {
    /// A reply needing nothing of the store.
    Reply(Reply),
    /// Statements the store runs.
    Statements(Planned),
    /// Work in more than one round trip, or on the connection itself.
    Procedure(Procedure),
}

/// The commands a connection carries out itself.
pub(crate) enum Procedure {
    Quit,
    Reset,
    Hello {
        protocol: Option<i64>,
        auth: Option<(Vec<u8>, Vec<u8>)>,
        name: Option<Vec<u8>>,
    },
    Auth {
        user: Option<Vec<u8>>,
        password: Vec<u8>,
    },
    Client(Vec<Vec<u8>>),
    Use(Space),
    Multi,
    Exec,
    Discard,
    /// `WATCH key …`: `EXEC` refused if any of them changes first.
    Watch(Vec<Vec<u8>>),
    Unwatch,
    Keys(Vec<u8>),
    Scan {
        cursor: u64,
        pattern: Option<Vec<u8>>,
        count: u64,
        /// Only keys holding this kind, as `TYPE` names it.
        kind: Option<String>,
    },
    RandomKey,
    /// A read, a change made in Rust, and a write that holds only if the key
    /// still holds what was read.
    Change {
        key: Vec<u8>,
        change: change::Change,
    },
    SetIfNoneExist(Vec<(Vec<u8>, Vec<u8>)>),
    /// Keys read together, and one of them written while it still holds what
    /// was read (`*STORE`, `LMPOP`, `ZMPOP`).
    Rewrite {
        keys: Vec<Vec<u8>>,
        compute: crate::surface::rewrite::Compute,
    },
    /// `LMPOP` / `ZMPOP`: pop `count` from the first of `keys` holding any.
    PopFirst {
        keys: Vec<Vec<u8>>,
        ranking: bool,
        front: bool,
        count: usize,
    },
    Rename {
        from: Vec<u8>,
        to: Vec<u8>,
        only_new: bool,
    },
    Copy {
        from: Vec<u8>,
        to: Vec<u8>,
        replace: bool,
    },
    ExpireWhen {
        key: Vec<u8>,
        at: Expiry,
        when: keys::When,
    },
    /// A blocking pop: its non-blocking `form`, tried again on every commit
    /// until it finds something or `wait` passes (`None` is no limit).
    Block {
        form: Command,
        wait: Option<std::time::Duration>,
        shape: Shape,
    },
    /// `SUBSCRIBE`, `PSUBSCRIBE`, `SSUBSCRIBE`.
    Listen {
        what: Listening,
        names: Vec<Vec<u8>>,
    },
    /// `UNSUBSCRIBE`, `PUNSUBSCRIBE`, `SUNSUBSCRIBE`; no names is all of them.
    Leave {
        what: Listening,
        names: Vec<Vec<u8>>,
    },
    /// `PUBLISH`, `SPUBLISH`.
    Publish {
        kind: crate::surface::channels::Kind,
        channel: Vec<u8>,
        message: Vec<u8>,
    },
    /// `PUBSUB CHANNELS | NUMSUB | NUMPAT | SHARDCHANNELS | SHARDNUMSUB`.
    Pubsub(Asked),
    /// A stream command, over the topic its key names.
    Stream(Stream),
    /// A stream group command, over the topic's group.
    Group(Group),
}

/// What a command needs to be read.
pub(crate) struct Context<'a> {
    /// The connection's space, if it has one.
    pub(crate) space: Option<&'a Space>,
}

/// Read `command` as a step; an error reply when it cannot be one.
pub(crate) fn plan(
    command: &Command,
    context: &Context<'_>,
    binder: &mut Binder,
) -> Result<Step, Reply> {
    let Some(first) = command.first() else {
        return Err(replies::unknown(b""));
    };
    let name = first.to_ascii_uppercase();
    let name = String::from_utf8_lossy(&name);
    let arguments = &command[1..];
    if let Some(step) = arguments::connection(&name, arguments)? {
        return Ok(step);
    }
    if let Some(step) = pubsub::plan(&name, arguments)? {
        return Ok(step);
    }
    let Some(space) = context.space else {
        return if strings::serves(&name)
            || keys::serves(&name)
            || hashes::serves(&name)
            || fields::serves(&name)
            || lists::serves(&name)
            || sets::serves(&name)
            || zsets::serves(&name)
            || combine::serves(&name)
            || scans::serves(&name)
            || pops::serves(&name)
            || blocking::serves(&name)
            || streams::serves(&name)
            || groups::serves(&name)
        {
            Err(replies::no_space())
        } else {
            Err(replies::unknown(first))
        };
    };
    if let Some(step) = strings::plan(&name, arguments, space, binder)? {
        return Ok(step);
    }
    if let Some(step) = keys::plan(&name, arguments, space, binder)? {
        return Ok(step);
    }
    if let Some(step) = blocking::plan(&name, arguments)? {
        return Ok(step);
    }
    if let Some(step) = streams::plan(&name, arguments)? {
        return Ok(step);
    }
    if let Some(step) = groups::plan(&name, arguments)? {
        return Ok(step);
    }
    for family in [
        hashes::plan,
        fields::plan,
        lists::plan,
        sets::plan,
        zsets::plan,
        combine::plan,
        scans::plan,
        pops::plan,
    ] {
        if let Some(step) = family(&name, arguments, space, binder)? {
            return Ok(step);
        }
    }
    Err(replies::unknown(first))
}
