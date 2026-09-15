//! Which log an entry belongs to, once a range can have more than one.
//!
//! A log used to be named by its home alone: the reach a record belongs to,
//! decided by the partition function above this layer. That was exact while one
//! leader decided every write into a range, because a range then had exactly one
//! counter and a position was unambiguous inside it.
//!
//! A range that admits two writers has two counters, and a position means
//! nothing without the counter it was allocated from. So a log is named by the
//! pair — the home, and the writer allocating into it.

use tessari_types::Reach;

use crate::node::NODE_ID_LEN;

/// The node that allocates positions into a log.
///
/// # Why this is the node's own identifier and not a small allocated number
///
/// Every other identifier in a key here is a dense number the catalog allocated
/// — a namespace, a database, a table — and they are numbers precisely so a key
/// stays short and a prefix stays fixed-width. This one is sixteen bytes, which
/// is a real cost repeated on every log entry for the life of the store, and it
/// is paid deliberately.
///
/// A dense number needs an allocator **everyone agrees with**. Under a single
/// leader that is free, because the leader is the agreement. A range with two
/// masters has no such authority by construction — the whole point of admitting
/// two writers is to stop requiring one — so two nodes would allocate
/// independently, and two nodes that allocated the same number would be writing
/// into **one** log while each believed it held its own. That is silent, it is
/// exactly the failure a per-writer log exists to prevent, and it would surface
/// as a divergence nobody could explain.
///
/// A self-describing identifier needs no agreement. The sixteen bytes are the
/// price of not needing one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Writer([u8; NODE_ID_LEN]);

impl Writer {
    /// The writer of an entry written before entries named their writer.
    ///
    /// A store migrated up from an unqualified log has no recorded writer for
    /// its existing entries, and the migrating node's own identifier would be a
    /// lie: a follower's log holds the records the *leader* wrote. So they are
    /// attributed to nobody, which is the only true statement available.
    ///
    /// It is a value rather than an absence on purpose — an `Option` here would
    /// put a branch on the read path of every log entry forever, to represent
    /// something that stops being produced the moment a store is migrated.
    pub const UNATTRIBUTED: Self = Self([0; NODE_ID_LEN]);

    /// The writer a node's own identifier names.
    #[must_use]
    pub const fn new(id: [u8; NODE_ID_LEN]) -> Self {
        Self(id)
    }

    /// The identifier, as the bytes a key carries.
    #[must_use]
    pub const fn bytes(self) -> [u8; NODE_ID_LEN] {
        self.0
    }
}

/// The log an entry belongs to: a home, and the writer allocating into it.
///
/// # Why the pair is a type rather than two arguments
///
/// Every method that reads or applies a log entry needs both halves, and a pair
/// passed as two parameters is a pair whose order can be got wrong at each of
/// them — silently, because a reach and a writer are different types only until
/// somebody writes a helper that takes both. As one value the compiler carries
/// the decision: a call site that has not been told about writers does not
/// compile, rather than reading the wrong log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LogId {
    /// The reach whose records this log carries.
    pub home: Reach,
    /// The node allocating positions into it.
    pub writer: Writer,
}

impl LogId {
    /// Name a log by its home and its writer.
    #[must_use]
    pub const fn new(home: Reach, writer: Writer) -> Self {
        Self { home, writer }
    }

    /// The log an unqualified entry of `home` belongs to.
    ///
    /// The migrated shape, and the one a caller that has no writer to name
    /// should reach for only when it is genuinely speaking about entries written
    /// before writers were named.
    #[must_use]
    pub const fn unattributed(home: Reach) -> Self {
        Self::new(home, Writer::UNATTRIBUTED)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use tessari_types::NamespaceId;

    use super::*;

    #[test]
    fn an_unattributed_writer_is_the_one_a_migration_writes() {
        assert_eq!(Writer::UNATTRIBUTED.bytes(), [0; NODE_ID_LEN]);
    }

    #[test]
    fn two_writers_in_one_home_are_two_logs() {
        let home = Reach::Namespace(NamespaceId::new(7));
        let one = LogId::new(home, Writer::new([1; NODE_ID_LEN]));
        let other = LogId::new(home, Writer::new([2; NODE_ID_LEN]));
        assert_ne!(one, other, "the same home does not make one log");
        assert_eq!(one.home, other.home);
    }
}
