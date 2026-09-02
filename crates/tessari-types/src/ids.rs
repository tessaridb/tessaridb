//! Numeric identifiers for the tenancy hierarchy and the store's sequence.
//!
//! Every identifier is a distinct type. The alternative — passing three `u32`s
//! in a row — compiles just as well while a transposed pair silently addresses
//! the wrong table, so the type system carries the distinction instead of the
//! argument order.
//!
//! Identifiers are numeric and fixed-width because they appear in the leading
//! bytes of every record key: a fixed-width prefix is what makes "everything in
//! one table" a prefix scan, and it keeps a rename out of the key entirely.

use core::fmt;

macro_rules! define_id {
    (
        $(#[$outer:meta])*
        $name:ident($inner:ty)
    ) => {
        $(#[$outer])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name($inner);

        impl $name {
            /// Wrap a raw identifier.
            #[must_use]
            pub const fn new(value: $inner) -> Self {
                Self(value)
            }

            /// The raw identifier.
            #[must_use]
            pub const fn get(self) -> $inner {
                self.0
            }
        }

        impl From<$inner> for $name {
            fn from(value: $inner) -> Self {
                Self(value)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0)
            }
        }
    };
}

define_id! {
    /// Identifies a namespace — the outermost tenancy level.
    NamespaceId(u32)
}

define_id! {
    /// Identifies a database within a namespace.
    DatabaseId(u32)
}

define_id! {
    /// Identifies a table within a database.
    TableId(u32)
}

define_id! {
    /// Identifies a graph within a database.
    ///
    /// A graph carries an id for a reason the other levels do not: the id sits in
    /// the leading bytes of every adjacency key, above the node it belongs to, so
    /// the whole structure is one prefix and removing it is one range delete
    /// rather than a scan. That is what makes a graph a thing the store can hold
    /// rather than a convention spread across several tables.
    GraphId(u32)
}

define_id! {
    /// Identifies an edge kind within a graph.
    ///
    /// An edge kind is not a table, so it cannot borrow a table's id: its entries
    /// are adjacency keys beside the node rather than records behind an index.
    /// The id sits between the node and the neighbour in every one of those keys,
    /// which is what makes *"this node's `works_at` edges"* a single bounded range
    /// rather than a scan of everything touching the node.
    EdgeKindId(u32)
}

define_id! {
    /// Identifies an index on a table.
    ///
    /// An index carries an id for the same reason a table does: the id is what
    /// every index entry's key holds, so renaming an index rewrites one catalog
    /// entry rather than every entry in it.
    IndexId(u32)
}

define_id! {
    /// Identifies a declared field on a table.
    ///
    /// A field carries an id even though nothing keys on it, because the catalog
    /// keys every definition by its own id and a field is a definition like any
    /// other. It also means renaming a field rewrites one entry.
    FieldId(u32)
}

/// A position in the store's ordered log.
///
/// One number serves two roles, deliberately: it is the log position and it is
/// the MVCC version. Keeping them identical is what makes a snapshot a single
/// number and makes replay reproduce exactly the visible history.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Sequence(u64);

impl Sequence {
    /// The position before anything has been written.
    pub const ZERO: Self = Self(0);

    /// Wrap a raw sequence number.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// The raw sequence number.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl From<u64> for Sequence {
    fn from(value: u64) -> Self {
        Self(value)
    }
}

impl fmt::Display for Sequence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_round_trip_their_raw_value() {
        assert_eq!(NamespaceId::new(7).get(), 7);
        assert_eq!(DatabaseId::from(9_u32).get(), 9);
        assert_eq!(TableId::new(u32::MAX).get(), u32::MAX);
        assert_eq!(Sequence::new(42).get(), 42);
    }

    #[test]
    fn the_zero_sequence_is_the_default_and_the_low_end() {
        assert_eq!(Sequence::ZERO, Sequence::default());
        assert!(Sequence::ZERO < Sequence::new(1));
    }

    #[test]
    fn ordering_follows_the_numeric_value() {
        let mut sequences = vec![Sequence::new(3), Sequence::new(1), Sequence::new(2)];
        sequences.sort_unstable();
        assert_eq!(
            sequences,
            vec![Sequence::new(1), Sequence::new(2), Sequence::new(3)]
        );
    }
}
