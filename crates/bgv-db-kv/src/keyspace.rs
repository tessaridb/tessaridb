//! Keyspaces — the fixed set of physically separated key domains.
//!
//! A keyspace is a named, independently tunable region of the store. Backends
//! that support physical separation (column families, separate trees) map a
//! keyspace onto one; backends that do not keep separate maps.
//!
//! # Why the set is fixed at compile time
//!
//! Adding a keyspace to a store that already holds data is not a code change —
//! it is a redeployment that must reopen every existing store with a new region
//! name, in lockstep, everywhere. The set is therefore declared once, up front,
//! including regions that are still empty.
//!
//! Tables are **not** keyspaces. A table is a prefix inside [`Keyspace::DATA`].
//! Keyspaces separate things with genuinely different access shapes — an
//! append-and-scan log behaves nothing like random point reads — so that each
//! can be tuned independently.

/// A named, physically separated region of the store.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Keyspace(&'static str);

impl Keyspace {
    /// Store metadata: on-disk format version, catalog state, migration
    /// watermarks.
    ///
    /// Present from the first open even while empty, because it cannot be added
    /// later without reopening every deployed store.
    pub const META: Self = Self("meta");

    /// User records.
    pub const DATA: Self = Self("data");

    /// Index entries pointing back into [`Self::DATA`].
    pub const INDEX: Self = Self("index");

    /// The ordered mutation log.
    ///
    /// Written sequentially and read by range, which is a different shape from
    /// every other keyspace and the reason this one is separate.
    pub const LOG: Self = Self("log");

    /// Every keyspace a store must open, in a stable order.
    pub const ALL: &'static [Self] = &[Self::META, Self::DATA, Self::INDEX, Self::LOG];

    /// The keyspace name as used by the backend.
    #[must_use]
    pub const fn name(self) -> &'static str {
        self.0
    }

    /// Resolve a keyspace by name, or `None` when it is not one of [`Self::ALL`].
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        let mut index = 0;
        while index < Self::ALL.len() {
            let candidate = Self::ALL[index];
            if candidate.0 == name {
                return Some(candidate);
            }
            index = index.saturating_add(1);
        }
        None
    }
}

impl std::fmt::Display for Keyspace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_keyspace_is_reachable_by_name() {
        for keyspace in Keyspace::ALL {
            assert_eq!(Keyspace::from_name(keyspace.name()), Some(*keyspace));
        }
    }

    #[test]
    fn unknown_names_do_not_resolve() {
        assert_eq!(Keyspace::from_name("nope"), None);
        assert_eq!(Keyspace::from_name(""), None);
    }

    #[test]
    fn names_are_unique_and_stable() {
        let names: Vec<&str> = Keyspace::ALL.iter().map(|k| k.name()).collect();
        assert_eq!(names, ["meta", "data", "index", "log"]);
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), names.len(), "keyspace names must be unique");
    }
}
