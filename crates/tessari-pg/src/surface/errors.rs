//! The correspondence between the store's refusals and the wire protocol's
//! error fields: the five-character codes as named constants, one mapping
//! from the store's refusal class, and the store's own words carried verbatim.
//!
//! The dialect's clients branch on the code and read the message; the store's
//! refusal NAME rides the detail field, so a refusal is never paraphrased.

use tessari_types::RefusalClass;
use tessaridb::Error;

/// The five-character codes this surface answers with.
pub(crate) mod code {
    /// The statement's shape is not this dialect's.
    pub(crate) const SYNTAX: &str = "42601";
    /// The shape is the dialect's but this build does not speak it yet.
    pub(crate) const FEATURE: &str = "0A000";
    /// The name or password did not sign in.
    pub(crate) const INVALID_PASSWORD: &str = "28P01";
    /// No user was presented where one is required.
    pub(crate) const INVALID_AUTHORIZATION: &str = "28000";
    /// The startup database does not name a namespace and a database.
    pub(crate) const INVALID_CATALOG: &str = "3D000";
    /// The store refused the statement; the class decides the code.
    pub(crate) const FORBIDDEN: &str = "42501";
    /// Asking again can succeed: a conflict, a moved shard, a busy store.
    pub(crate) const SERIALIZATION: &str = "40001";
    /// The store is not reachable for this read right now.
    pub(crate) const UNAVAILABLE: &str = "57P03";
    /// The caller is being made to wait.
    pub(crate) const THROTTLED: &str = "55P03";
    /// A Bind named a statement that was not prepared, or a Describe or an
    /// Execute named a portal that is not open.
    pub(crate) const UNDEFINED_STATEMENT: &str = "26000";
    /// A Describe or an Execute named a portal that is not open.
    pub(crate) const UNDEFINED_CURSOR: &str = "34000";
    /// The messages did not add up: the values did not match the statement's
    /// parameters, or a name was asked for in two kinds.
    pub(crate) const PROTOCOL: &str = "08P01";
    /// The surface has as many named statements or portals open as it takes.
    pub(crate) const LIMIT: &str = "53400";
    /// A text value did not read as the type it was declared under.
    pub(crate) const INVALID_TEXT: &str = "22P02";
    /// Everything else, including what this surface does not carry yet.
    pub(crate) const INTERNAL: &str = "XX000";
}

/// One refusal rendered for the wire: the code, the message — the store's own
/// words, verbatim — and the detail carrying the refusal's name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Refused {
    pub(crate) code: &'static str,
    pub(crate) message: String,
    pub(crate) detail: Option<String>,
}

impl Refused {
    /// A refusal of this surface's own: the translator's syntax and feature
    /// refusals, which have no store refusal behind them.
    pub(crate) fn surface(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            detail: None,
        }
    }

    /// The store's refusal, coded by its class, worded by the store.
    pub(crate) fn store(error: &Error) -> Self {
        Self {
            code: class_code(error.class()),
            message: error.to_string(),
            detail: Some(name_of(error)),
        }
    }

    /// A refusal that arrived over the peer link under a carried call: the
    /// class and the answering node's words travel together, and the name is
    /// the link's own — there is no local variant to carry.
    pub(crate) fn carried(class: RefusalClass, message: String) -> Self {
        Self {
            code: class_code(class),
            message,
            detail: None,
        }
    }

    /// A named surface refusal with the detail filled — what was found and
    /// where, so a client can show it without a round trip.
    pub(crate) fn with_detail(
        code: &'static str,
        message: impl Into<String>,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            code,
            message: message.into(),
            detail: Some(detail.into()),
        }
    }
}

/// The class decides the code: a caller of the dialect retries exactly what
/// the store's own clients retry, and no more.
fn class_code(class: RefusalClass) -> &'static str {
    match class {
        RefusalClass::Invalid => code::SYNTAX,
        RefusalClass::Unauthenticated => code::INVALID_PASSWORD,
        RefusalClass::Forbidden => code::FORBIDDEN,
        RefusalClass::Throttled => code::THROTTLED,
        RefusalClass::Elsewhere => code::INTERNAL,
        RefusalClass::Retry => code::SERIALIZATION,
        RefusalClass::Conflict => code::SERIALIZATION,
        RefusalClass::Unavailable => code::UNAVAILABLE,
        RefusalClass::Internal => code::INTERNAL,
    }
}

/// The refusal's name: the variant discriminant of the store's error, which
/// is what a test asserts on and what the detail field exists to carry.
fn name_of(error: &Error) -> String {
    let debug = format!("{error:?}");
    let end = debug.find([' ', '(', '{']).unwrap_or(debug.len());
    debug[..end].to_owned()
}

#[cfg(test)]
mod tests;
