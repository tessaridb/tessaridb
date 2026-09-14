//! How far an authority reaches.
//!
//! The shape and its order live here rather than beside the authority model that
//! first needed them, because a log key carries a reach and store keys are
//! encoded a layer below that model. A type cannot be referenced from underneath
//! the crate that defines it, and the alternative — a second reach-shaped type in
//! the encoding layer — would be one fact in two places, which is the defect this
//! store keeps finding rather than one it should add.
//!
//! What stays behind is the catalog codec: it turns a reach into a stored
//! [`Value`] and back, and it raises the storage layer's own malformed-catalog
//! error, so it belongs with the catalog and not here.
//!
//! [`Value`]: crate::Value

use crate::{DatabaseId, NamespaceId};

/// How far an authority reaches.
///
/// [`Self::Database`] carries its namespace as well as its database, so a
/// database reach cannot be constructed without the namespace that contains it.
/// The alternative — two `Option` fields — makes "a database in no namespace"
/// a value somebody has to remember to reject.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Reach {
    /// The whole store, every namespace in it.
    Store,
    /// One namespace, every database in it.
    Namespace(NamespaceId),
    /// One database.
    Database(NamespaceId, DatabaseId),
}

impl Reach {
    /// The reach a user's tenancy describes.
    ///
    /// `None` for a database named without a namespace, which is not a place —
    /// the caller decides whether that is corruption or a bad request, because
    /// this type cannot tell which of its callers it is answering.
    #[must_use]
    pub const fn of(namespace: Option<NamespaceId>, database: Option<DatabaseId>) -> Option<Self> {
        match (namespace, database) {
            (None, None) => Some(Self::Store),
            (Some(namespace), None) => Some(Self::Namespace(namespace)),
            (Some(namespace), Some(database)) => Some(Self::Database(namespace, database)),
            (None, Some(_)) => None,
        }
    }

    /// The tenancy this reach describes — the inverse of [`Self::of`].
    ///
    /// Paired with `of` so that the two directions cannot drift: a caller
    /// holding a reach never has to rebuild the pair by hand and never has to
    /// handle the database-without-a-namespace case, which this type makes
    /// unconstructible.
    #[must_use]
    pub const fn parts(self) -> (Option<NamespaceId>, Option<DatabaseId>) {
        match self {
            Self::Store => (None, None),
            Self::Namespace(namespace) => (Some(namespace), None),
            Self::Database(namespace, database) => (Some(namespace), Some(database)),
        }
    }

    /// Whether this reach contains `other`.
    ///
    /// The only implication in this model. Downward and nothing else: the store
    /// contains a namespace, a namespace contains its databases, and a reach
    /// contains itself.
    #[must_use]
    pub fn contains(self, other: Self) -> bool {
        match (self, other) {
            (Self::Store, _) => true,
            (Self::Namespace(mine), Self::Namespace(theirs)) => mine == theirs,
            (Self::Namespace(mine), Self::Database(theirs, _)) => mine == theirs,
            (Self::Database(namespace, database), Self::Database(theirs, their_database)) => {
                namespace == theirs && database == their_database
            }
            // A database reach does not contain the namespace above it, and a
            // namespace reach does not contain the store. Written out rather
            // than left to a catch-all so that adding a reach fails to compile
            // here instead of silently answering `false`.
            (Self::Namespace(_) | Self::Database(_, _), Self::Store)
            | (Self::Database(_, _), Self::Namespace(_)) => false,
        }
    }

    /// The narrowest reach containing both.
    ///
    /// [`Self::contains`] makes these three variants a containment order with
    /// [`Self::Store`] at the top, and this is that order's join: the least
    /// reach that covers `self` and `other` at once. It is total — every pair
    /// has one, because `Store` covers everything — which is what lets a log
    /// record carrying mutations from several tenancies be filed in one place
    /// instead of split or duplicated.
    ///
    /// Two databases in one namespace join at that namespace; two namespaces
    /// join at the store; anything joins with itself.
    ///
    /// Written as an exhaustive match for the same reason [`Self::contains`] is:
    /// a fourth reach must fail to compile here rather than fall into a
    /// catch-all and silently answer `Store`, which would be a correct-looking
    /// answer that files every record at the top.
    #[must_use]
    pub fn join(self, other: Self) -> Self {
        match (self, other) {
            (Self::Store, _) | (_, Self::Store) => Self::Store,
            (Self::Namespace(mine), Self::Namespace(theirs)) => {
                if mine == theirs {
                    Self::Namespace(mine)
                } else {
                    Self::Store
                }
            }
            (Self::Namespace(mine), Self::Database(theirs, _))
            | (Self::Database(theirs, _), Self::Namespace(mine)) => {
                if mine == theirs {
                    Self::Namespace(mine)
                } else {
                    Self::Store
                }
            }
            (Self::Database(mine, my_database), Self::Database(theirs, their_database)) => {
                if mine != theirs {
                    Self::Store
                } else if my_database == their_database {
                    Self::Database(mine, my_database)
                } else {
                    Self::Namespace(mine)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROD: NamespaceId = NamespaceId::new(3);
    const OTHER: NamespaceId = NamespaceId::new(4);
    const LIBRARY: DatabaseId = DatabaseId::new(7);
    const ARCHIVE: DatabaseId = DatabaseId::new(8);

    #[test]
    fn two_databases_in_one_namespace_join_at_that_namespace() {
        let library = Reach::Database(PROD, LIBRARY);
        let archive = Reach::Database(PROD, ARCHIVE);
        assert_eq!(library.join(archive), Reach::Namespace(PROD));
        assert_eq!(archive.join(library), Reach::Namespace(PROD));
    }

    #[test]
    fn two_namespaces_have_nothing_below_the_store_in_common() {
        assert_eq!(
            Reach::Namespace(PROD).join(Reach::Namespace(OTHER)),
            Reach::Store
        );
        assert_eq!(
            Reach::Database(PROD, LIBRARY).join(Reach::Database(OTHER, LIBRARY)),
            Reach::Store,
            "the same database id under two namespaces is two different places"
        );
    }

    #[test]
    fn a_reach_joined_with_one_it_contains_is_itself() {
        // The join is what lets a record carrying several tenancies be filed in
        // one place, so the case that must not widen is the common one: writes
        // that already sit under a single container.
        let namespace = Reach::Namespace(PROD);
        assert_eq!(namespace.join(Reach::Database(PROD, LIBRARY)), namespace);
        assert_eq!(Reach::Database(PROD, LIBRARY).join(namespace), namespace);
        assert_eq!(Reach::Store.join(namespace), Reach::Store);
    }

    #[test]
    fn joining_a_reach_with_itself_changes_nothing() {
        for reach in [
            Reach::Store,
            Reach::Namespace(PROD),
            Reach::Database(PROD, LIBRARY),
        ] {
            assert_eq!(reach.join(reach), reach);
        }
    }

    #[test]
    fn the_join_is_the_least_reach_containing_both() {
        // The property, rather than the table above: whatever `join` answers
        // contains both inputs, and nothing it contains strictly does.
        let every = [
            Reach::Store,
            Reach::Namespace(PROD),
            Reach::Namespace(OTHER),
            Reach::Database(PROD, LIBRARY),
            Reach::Database(PROD, ARCHIVE),
            Reach::Database(OTHER, LIBRARY),
        ];
        for left in every {
            for right in every {
                let joined = left.join(right);
                assert!(joined.contains(left), "{joined:?} must contain {left:?}");
                assert!(joined.contains(right), "{joined:?} must contain {right:?}");
                for candidate in every {
                    if joined.contains(candidate) && candidate != joined {
                        assert!(
                            !(candidate.contains(left) && candidate.contains(right)),
                            "{candidate:?} is narrower than {joined:?} and covers both"
                        );
                    }
                }
            }
        }
    }
}
