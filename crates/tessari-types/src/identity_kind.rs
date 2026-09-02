//! How a table names a record the caller did not name.
//!
//! A write may say which record it means — `CREATE users:1 = { … }` — or leave
//! that to the store — `CREATE users = { … }`. This says what the store produces
//! when it is left to it, and it is a property of the **table** rather than of
//! the statement, because two records in one table named on two different
//! schemes would sort into two regions and read back as one table only by
//! accident.
//!
//! # Why the default is a sequence
//!
//! `docs/key-grammar.md` §5 encodes [`crate::RecordId::Int`] as eight bytes,
//! sign-flipped big-endian and fixed width. That is order-preserving, so records
//! named by a counter sort in the order they were written and a read of the most
//! recent ones is a bounded scan of adjacent keys. A random identifier scatters
//! them across the keyspace, which costs on every range read for a property most
//! tables do not need.
//!
//! # Why the other one exists
//!
//! A counter is allocated, and allocation is coordination: one hot key per table,
//! paid as a transaction conflict when many writers arrive at once. It also
//! *publishes* how many records there are — the holder of `users:41` knows there
//! are at least forty-one and can ask for `users:42`. A table whose identities
//! are reachable by an untrusted caller — a session, a token, an invitation,
//! anything a URL carries — declares [`Self::Uuid`] and pays scattered writes to
//! get neither property.
//!
//! Neither is a default the other can be recovered from afterwards: records
//! already written keep the names they were given.

use core::fmt;

/// What a table names a record with when the caller does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub enum IdentityKind {
    /// A per-table counter, stored as [`crate::RecordId::Int`].
    ///
    /// The default. Records sort in the order they were written.
    #[default]
    Int,
    /// A UUID version 7, stored as [`crate::RecordId::Uuid`].
    ///
    /// Minted without coordination and without disclosing how many records the
    /// table holds.
    Uuid,
}

impl IdentityKind {
    /// The word that declares it, and the word the catalog stores.
    ///
    /// One spelling for both, so a definition that round-trips through
    /// `INFO FOR TABLE` comes back as the statement that created it.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Int => "int",
            Self::Uuid => "uuid",
        }
    }

    /// Read the word back.
    ///
    /// Answers `None` for anything else rather than falling back to the default:
    /// a table stored under a scheme this build does not know about must not be
    /// read as though it used the one this build happens to prefer.
    #[must_use]
    pub fn parse(word: &str) -> Option<Self> {
        match word {
            "int" => Some(Self::Int),
            "uuid" => Some(Self::Uuid),
            _ => None,
        }
    }

    /// Every kind, for the tests that assert a match is exhaustive.
    ///
    /// The same guard the function and field-kind vocabularies carry: a variant
    /// added without being taught to the catalog, the parser and the documentation
    /// is a variant that silently means the default somewhere.
    pub const ALL: &'static [Self] = &[Self::Int, Self::Uuid];
}

impl fmt::Display for IdentityKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

#[cfg(test)]
mod tests {
    use super::IdentityKind;

    #[test]
    fn a_table_that_says_nothing_gets_a_sequence() {
        assert_eq!(IdentityKind::default(), IdentityKind::Int);
    }

    #[test]
    fn every_kind_round_trips_through_its_word() {
        for kind in IdentityKind::ALL {
            assert_eq!(IdentityKind::parse(kind.name()), Some(*kind), "{kind}");
        }
    }

    #[test]
    fn an_unknown_word_is_not_quietly_the_default() {
        // The failure this refuses is a store written by a later build being
        // read as `int` by an earlier one, which would name new records into a
        // scheme the table is not using.
        assert_eq!(IdentityKind::parse("ulid"), None);
        assert_eq!(IdentityKind::parse(""), None);
        assert_eq!(IdentityKind::parse("Int"), None);
    }
}
