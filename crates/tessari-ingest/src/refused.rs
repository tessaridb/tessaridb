//! Why a message could not be shaped into a record.
//!
//! The operator is who reads these — in a quarantine record beside the payload,
//! or as the reason a `stop` consumer halted — so each variant's `Display` is
//! the sentence they read. The variants are what code and tests decide on.

use core::fmt;

use crate::json::Malformed;

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
