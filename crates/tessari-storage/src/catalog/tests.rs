#![allow(clippy::unwrap_used)]

use super::*;

/// The two directions of one format, asserted against each other.
///
/// Written as a round trip rather than against literals because a literal
/// pins what somebody typed and a round trip pins what `qualify` produces —
/// and the reader exists to read exactly that.
#[test]
fn a_qualified_name_reads_back_as_the_level_and_parents_it_was_built_from() {
    for (level, parents) in [
        (Level::Namespace, vec![]),
        (Level::Database, vec![7]),
        (Level::Table, vec![7, 3]),
        (Level::Index, vec![7, 3, 12]),
        (Level::Field, vec![7, 3, 12]),
        (Level::Graph, vec![7, 3]),
        (Level::EdgeKind, vec![7, 3]),
        (Level::Analyzer, vec![]),
        (Level::User, vec![]),
        (Level::Replica, vec![]),
        (Level::Consumer, vec![]),
    ] {
        let qualified = qualify(level, &parents, "orders");
        assert_eq!(
            parse_qualified(&qualified),
            Some((level, parents.clone())),
            "{qualified} must read back as what built it"
        );
    }
}

/// A name that itself begins with a number and a slash is the case the level
/// tag was introduced for, and the reader must not mistake it for a parent
/// it does not have. Over-reading is harmless because the true parents are
/// always leftmost — but that is an argument, and this is the evidence.
#[test]
fn a_name_that_looks_like_a_parent_does_not_move_the_real_ones() {
    let qualified = qualify(Level::Table, &[7, 3], "9/orders");
    let (level, parents) = parse_qualified(&qualified).unwrap();
    assert_eq!(level, Level::Table);
    assert_eq!(parents.first(), Some(&7));
    assert_eq!(parents.get(1), Some(&3));
}

/// Anything this build did not write reads as *cannot tell*, never as a
/// guess — the caller is the replication filter and its answer to that is to
/// withhold.
#[test]
fn an_unknown_qualified_name_reads_as_cannot_tell() {
    assert_eq!(parse_qualified("orders"), None);
    assert_eq!(parse_qualified("zz:7/orders"), None);
}

#[test]
fn the_level_tag_keeps_a_namespace_name_from_colliding_with_a_database_name() {
    let namespace = qualify(Level::Namespace, &[], "5/orders");
    let database = qualify(Level::Database, &[5], "orders");
    assert_ne!(namespace, database);
}

#[test]
fn the_same_name_in_two_databases_qualifies_differently() {
    assert_ne!(
        qualify(Level::Table, &[1, 2], "users"),
        qualify(Level::Table, &[1, 3], "users")
    );
}
