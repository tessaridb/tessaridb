//! Which of the three record writes happened — what an event is asked about
//! (ADR-0110).
//!
//! In the types crate rather than the language or the catalog because both
//! hold it: the parser reads `FOR CREATE, UPDATE`, the catalog stores it, and
//! the session binds it to `$event`. One spelling for all three is what keeps
//! a stored event meaning what was written.

use core::fmt;

/// One kind of record write.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum WriteKind {
    /// A record that was not there now is.
    Create,
    /// A record that was there was written again.
    Update,
    /// A record that was there is gone.
    Delete,
}

impl WriteKind {
    /// All three, in the order `INFO` writes them.
    pub const ALL: [Self; 3] = [Self::Create, Self::Update, Self::Delete];

    /// The word, as written in a statement and bound to `$event`.
    #[must_use]
    pub const fn word(self) -> &'static str {
        match self {
            Self::Create => "CREATE",
            Self::Update => "UPDATE",
            Self::Delete => "DELETE",
        }
    }

    /// The kind a word names, in any case.
    #[must_use]
    pub fn from_word(word: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|kind| kind.word().eq_ignore_ascii_case(word))
    }
}

impl fmt::Display for WriteKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.word())
    }
}

#[cfg(test)]
mod tests {
    use super::WriteKind;

    #[test]
    fn every_kind_reads_back_from_its_word() {
        for kind in WriteKind::ALL {
            assert_eq!(WriteKind::from_word(kind.word()), Some(kind));
            assert_eq!(
                WriteKind::from_word(&kind.word().to_lowercase()),
                Some(kind)
            );
        }
        assert_eq!(WriteKind::from_word("upsert"), None);
    }
}
