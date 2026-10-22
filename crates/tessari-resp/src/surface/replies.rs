//! The error replies a command answers, their first word the class a client
//! branches on.

use std::borrow::Cow;

use tessari_types::RefusalClass;

use crate::codec::Reply;
use crate::surface::running::Failed;

/// Wrong number of arguments for `name`.
pub(crate) fn arity(name: &str) -> Reply {
    Reply::error(format!(
        "ERR wrong number of arguments for '{}' command",
        name.to_ascii_lowercase()
    ))
}

/// A word in a place no word of that command goes.
pub(crate) fn syntax() -> Reply {
    Reply::error("ERR syntax error")
}

/// An argument that is not the integer it must be.
pub(crate) fn not_an_integer() -> Reply {
    Reply::error("ERR value is not an integer or out of range")
}

/// An argument that is not the float it must be.
pub(crate) fn not_a_float() -> Reply {
    Reply::error("ERR value is not a valid float")
}

/// A key holding a value no string stands for.
pub(crate) fn wrong_type() -> Reply {
    Reply::error("WRONGTYPE Operation against a key holding the wrong kind of value")
}

/// A command this node does not serve.
pub(crate) fn unknown(name: &[u8]) -> Reply {
    Reply::error(format!(
        "ERR unknown command '{}'",
        String::from_utf8_lossy(name)
    ))
}

/// A command run with no space chosen.
pub(crate) fn no_space() -> Reply {
    Reply::error(
        "ERR no space is chosen: the node was started without --resp-space, \
         send TESSARIDB.USE <namespace> <database> <space>",
    )
}

/// The reply the protocol gives in its own words for a container write the
/// store refused, where it has one.
pub(crate) fn container(refused: &tessaridb::Error) -> Option<Reply> {
    match refused {
        tessaridb::Error::KeyHoldsAnotherKind { .. } => Some(wrong_type()),
        tessaridb::Error::NoSuchList { .. } => Some(Reply::error("ERR no such key")),
        tessaridb::Error::ListIndexOutOfRange { .. } => {
            Some(Reply::error("ERR index out of range"))
        }
        _ => None,
    }
}

/// A statement that did not answer, as the reply a client can act on: the
/// class decides the first word, the store's own words follow.
pub(crate) fn failed(failed: &Failed) -> Reply {
    if let Failed::Refused(refused) = failed
        && let Some(reply) = container(refused)
    {
        return reply;
    }
    match failed {
        Failed::Refused(refused) => {
            Reply::Error(Cow::Owned(format!("{} {refused}", word(refused.class()))))
        }
        Failed::Carried(refused) => Reply::Error(Cow::Owned(format!(
            "{} {}",
            word(refused.class),
            refused.message
        ))),
        Failed::Busy => Reply::error("TRYAGAIN the node is busy; try again"),
        Failed::Lost => Reply::error("ERR the command was lost; reconnect"),
    }
}

/// The first word of a refusal's reply, by its class.
const fn word(class: RefusalClass) -> &'static str {
    match class {
        RefusalClass::Unauthenticated => "NOAUTH",
        RefusalClass::Forbidden => "NOPERM",
        RefusalClass::Retry
        | RefusalClass::Unavailable
        | RefusalClass::Elsewhere
        | RefusalClass::Throttled => "TRYAGAIN",
        RefusalClass::Invalid | RefusalClass::Conflict | RefusalClass::Internal => "ERR",
    }
}

/// The refusal of a command `MULTI` cannot queue.
pub(crate) fn not_in_multi(command: &crate::codec::Command) -> Reply {
    let name = command
        .first()
        .map(|name| String::from_utf8_lossy(name).to_ascii_lowercase())
        .unwrap_or_default();
    Reply::error(format!(
        "ERR '{name}' is not offered inside MULTI on this node"
    ))
}

/// The text of an error reply; empty for any other.
pub(crate) fn error_text(reply: &Reply) -> String {
    match reply {
        Reply::Error(text) => text.to_string(),
        _ => String::new(),
    }
}
