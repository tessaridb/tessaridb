//! Why a part of a transaction across leaders was not done, kept as the kind
//! of no it was (Q-924).
//!
//! The kind is the one thing about a refusal a client acts on — ask again, or
//! not — and the HTTP surface answers it as a status. A refusal from this node
//! is kept whole, so it answers exactly as it would have outside a transaction
//! across leaders. A refusal from another node arrives as words, so its kind
//! crosses the peer link beside them: judged there, by the same mapping, and
//! never guessed here from the text.

use core::fmt;

use crate::error::Error;

/// The kind of no a leader gave its part.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefusalKind {
    /// Asking again can succeed: a conflict, a lapsed record, a leader that was
    /// not reached or did not answer in time.
    Retriable,
    /// The caller may not do this there, and asking again changes nothing.
    Forbidden,
    /// The writes themselves are refused, and asking again changes nothing.
    Invalid,
}

impl RefusalKind {
    /// The byte that names this kind on the peer link.
    const fn tag(self) -> u8 {
        match self {
            Self::Retriable => 0,
            Self::Forbidden => 1,
            Self::Invalid => 2,
        }
    }

    /// The kind a byte names, if it names one.
    const fn from_tag(tag: u8) -> Option<Self> {
        match tag {
            0 => Some(Self::Retriable),
            1 => Some(Self::Forbidden),
            2 => Some(Self::Invalid),
            _ => None,
        }
    }
}

/// Another node's refusal of a part, as it crossed the peer link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartRefused {
    /// The kind of no it was, judged on the node that refused.
    pub kind: RefusalKind,
    /// The refusal in that node's words.
    pub reason: String,
}

impl PartRefused {
    /// A refusal asking again can get past — the link's, or a leader's that
    /// did not answer in time.
    #[must_use]
    pub fn retriable(reason: impl Into<String>) -> Self {
        Self {
            kind: RefusalKind::Retriable,
            reason: reason.into(),
        }
    }

    /// Its bytes on the peer link: the kind, then the words.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(self.reason.len().saturating_add(1));
        bytes.push(self.kind.tag());
        bytes.extend_from_slice(self.reason.as_bytes());
        bytes
    }

    /// Read one back; `None` for bytes that do not begin with a kind.
    #[must_use]
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let (&tag, words) = bytes.split_first()?;
        Some(Self {
            kind: RefusalKind::from_tag(tag)?,
            reason: String::from_utf8_lossy(words).into_owned(),
        })
    }
}

/// Why a part of a transaction across leaders was not done.
#[derive(Debug)]
pub enum AcrossRefusal {
    /// This node's own refusal, kept whole.
    Here(Box<Error>),
    /// Another node's, as its words and its kind.
    There(PartRefused),
}

impl fmt::Display for AcrossRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Here(error) => error.fmt(f),
            Self::There(refused) => f.write_str(&refused.reason),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{PartRefused, RefusalKind};

    #[test]
    fn a_refusal_crosses_the_link_with_its_kind() {
        for kind in [
            RefusalKind::Retriable,
            RefusalKind::Forbidden,
            RefusalKind::Invalid,
        ] {
            let refused = PartRefused {
                kind,
                reason: "b: a write conflicts — désolé".to_owned(),
            };
            assert_eq!(PartRefused::decode(&refused.encode()), Some(refused));
        }
    }

    #[test]
    fn bytes_that_name_no_kind_are_not_a_refusal() {
        assert_eq!(PartRefused::decode(&[]), None);
        assert_eq!(PartRefused::decode(&[9, b'x']), None);
    }
}
