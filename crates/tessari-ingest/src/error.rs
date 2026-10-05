//! Why a message could not be shaped into a record.
//!
//! The operator is who reads these — in a quarantine record beside the payload,
//! or as the reason a `stop` consumer halted — so each variant's `Display` is
//! the sentence they read. The variants are what code and tests decide on.

use core::fmt;

/// Why [`crate::shape`] refused a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShapeRefused {
    /// The payload is not one JSON value.
    NotJson(Malformed),
    /// The declared identity field is not a route this store can follow.
    IdentityRouteInvalid {
        /// The identity field as declared.
        identity: String,
    },
    /// The message does not carry the identity field.
    IdentityMissing {
        /// The identity field as declared.
        identity: String,
    },
    /// The identity field holds something that cannot be a record identity.
    IdentityNotAnId {
        /// The identity field as declared.
        identity: String,
        /// The kind of value it held.
        found: &'static str,
    },
    /// A mapped source field is not a route this store can follow.
    MappingRouteInvalid {
        /// The source field as declared.
        from: String,
    },
}

impl fmt::Display for ShapeRefused {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotJson(failure) => write!(formatter, "the payload is not JSON: {failure}"),
            Self::IdentityRouteInvalid { identity } => write!(
                formatter,
                "the identity field {identity:?} is not a route this store can follow"
            ),
            Self::IdentityMissing { identity } => write!(
                formatter,
                "the message has no {identity:?}, which is the field the identity is taken from"
            ),
            Self::IdentityNotAnId { identity, found } => write!(
                formatter,
                "{identity:?} holds {found}, and a record identity is a whole number or a string"
            ),
            Self::MappingRouteInvalid { from } => {
                write!(formatter, "{from:?} is not a route this store can follow")
            }
        }
    }
}

impl std::error::Error for ShapeRefused {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::NotJson(failure) => Some(failure),
            _ => None,
        }
    }
}

/// Why a source could not do what was asked.
///
/// A string rather than an enum, because what can go wrong is the client's
/// vocabulary and not this crate's: inventing categories here would mean
/// mapping every client's failures onto a set chosen before any of them were
/// read, and the operator needs the client's own words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceError(pub String);

impl std::fmt::Display for SourceError {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.write_str(&self.0)
    }
}

impl std::error::Error for SourceError {}

/// Why a message could not be read — the shared JSON reader's own failure.
pub use tessari_types::json::Malformed;
