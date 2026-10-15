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

/// A table's indexes and fields found through its own names are exactly what
/// reading every definition in the store and keeping that table's answers —
/// for live tables, for dropped ones, and for a table whose id is a prefix of
/// another's digits, which is the one place a name span could reach too far.
#[test]
fn a_tables_definitions_found_by_its_names_are_the_whole_catalogs_answer() {
    use crate::{FieldShape, IndexShape, Store, TableShape};
    use std::sync::Arc;
    use tessari_types::{FieldKind, Path};

    let store = Store::open(Arc::new(tessari_kv::MemoryBackend::new())).unwrap();
    let mut seed = 0x2545_f491_u64;
    let mut roll = |below: u64| {
        seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
        (seed >> 33) % below
    };
    let mut transaction = store.begin().unwrap();
    let mut catalog = Catalog::new(&mut transaction);
    let mut tables = Vec::new();
    for space in ["a", "b"] {
        let namespace = catalog.create_namespace(space).unwrap();
        for base in ["x", "y"] {
            let database = catalog.create_database(namespace.id, base).unwrap();
            for nth in 0..60 {
                let table = catalog
                    .create_table(
                        namespace.id,
                        database.id,
                        &format!("t{nth}"),
                        TableShape::default(),
                    )
                    .unwrap();
                for index in 0..roll(4) {
                    catalog
                        .create_index(
                            table.id,
                            &format!("by_{}", 9 - index),
                            vec![Path::field("a")],
                            IndexShape::default(),
                        )
                        .unwrap();
                }
                for field in 0..roll(3) {
                    catalog
                        .create_field(
                            table.id,
                            &format!("f{}", 9 - field),
                            FieldKind::Any,
                            FieldShape::default(),
                        )
                        .unwrap();
                }
                tables.push(table.id);
            }
        }
    }
    transaction.commit().unwrap();
    let mut transaction = store.begin().unwrap();
    let mut catalog = Catalog::new(&mut transaction);
    for table in &tables {
        match roll(4) {
            0 => drop(catalog.drop_table(*table).unwrap()),
            1 => {
                for index in catalog.indexes_on(*table).unwrap() {
                    catalog.drop_index(index.id).unwrap();
                }
            }
            _ => {}
        }
    }
    transaction.commit().unwrap();

    let mut transaction = store.begin().unwrap();
    let catalog = Catalog::new(&mut transaction);
    let fields = catalog.fields().unwrap();
    let (mut dropped_with_children, mut live_with_indexes) = (0, Vec::new());
    for table in &tables {
        let mut whole = fields.clone();
        whole.retain(|field| field.table == *table);
        let indexes = catalog.indexes_on(*table).unwrap();
        assert_eq!(
            indexes,
            catalog.every_index_on(*table).unwrap(),
            "indexes of {table:?}"
        );
        assert_eq!(
            catalog.fields_on(*table).unwrap(),
            whole,
            "fields of {table:?}"
        );
        let present = catalog.table(*table).unwrap().is_some();
        if !present && !(indexes.is_empty() && whole.is_empty()) {
            dropped_with_children += 1;
        }
        if present && !indexes.is_empty() {
            live_with_indexes.push(table.get().to_string());
        }
    }
    for namespace in catalog.namespaces().unwrap() {
        let mut whole = catalog
            .all(system::DATABASES, DatabaseDefinition::from_value)
            .unwrap();
        whole.retain(|found| found.namespace == namespace.id);
        assert_eq!(
            catalog.databases_in(namespace.id).unwrap(),
            whole,
            "databases of {namespace:?}"
        );
        for database in whole {
            let mut tables = catalog
                .all(system::TABLES, TableDefinition::from_value)
                .unwrap();
            tables.retain(|found| found.namespace == namespace.id && found.database == database.id);
            assert!(!tables.is_empty(), "the control: a database with tables");
            assert_eq!(
                catalog.tables_in(namespace.id, database.id).unwrap(),
                tables,
                "tables of {database:?}"
            );
        }
    }
    // The controls: both paths were taken, and a digit-prefix pair was there to
    // be confused (ids are store-wide, so 240 tables span two and three digits).
    assert!(
        dropped_with_children > 0,
        "no dropped table kept definitions"
    );
    assert!(
        live_with_indexes.iter().any(|short| live_with_indexes
            .iter()
            .any(|long| long != short && long.starts_with(short.as_str()))),
        "no live table's id was a digit prefix of another's"
    );
}
