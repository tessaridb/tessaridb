#![allow(clippy::unwrap_used)]

use super::*;
use crate::now::Now;

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

    let store = Store::open(Arc::new(tessari_kv::MemoryBackend::new()))
        .now()
        .unwrap();
    let mut seed = 0x2545_f491_u64;
    let mut roll = |below: u64| {
        seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
        (seed >> 33) % below
    };
    let mut transaction = store.begin().now().unwrap();
    let mut catalog = Catalog::new(&mut transaction);
    let mut tables = Vec::new();
    for space in ["a", "b"] {
        let namespace = catalog.create_namespace(space).now().unwrap();
        for base in ["x", "y"] {
            let database = catalog.create_database(namespace.id, base).now().unwrap();
            for nth in 0..60 {
                let table = catalog
                    .create_table(
                        namespace.id,
                        database.id,
                        &format!("t{nth}"),
                        TableShape::default(),
                    )
                    .now()
                    .unwrap();
                for index in 0..roll(4) {
                    catalog
                        .create_index(
                            table.id,
                            &format!("by_{}", 9 - index),
                            vec![Path::field("a")],
                            IndexShape::default(),
                        )
                        .now()
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
                        .now()
                        .unwrap();
                }
                tables.push((namespace.id, database.id, table.id));
            }
        }
    }
    transaction.commit().now().unwrap();
    let mut transaction = store.begin().now().unwrap();
    let mut catalog = Catalog::new(&mut transaction);
    for &(namespace, database, table) in &tables {
        match roll(4) {
            0 => drop(catalog.drop_table(table).now().unwrap()),
            1 => {
                for index in catalog
                    .indexes_on(namespace, database, table)
                    .now()
                    .unwrap()
                {
                    catalog.drop_index(index.id).now().unwrap();
                }
            }
            _ => {}
        }
    }
    transaction.commit().now().unwrap();

    let mut transaction = store.begin().now().unwrap();
    let catalog = Catalog::new(&mut transaction);
    let fields = catalog.fields().now().unwrap();
    let (mut dropped_with_children, mut live_with_indexes) = (0, Vec::new());
    for &(namespace, database, table) in &tables {
        let mut whole = fields.clone();
        whole.retain(|field| field.table == table);
        let indexes = catalog
            .indexes_on(namespace, database, table)
            .now()
            .unwrap();
        assert_eq!(
            indexes,
            catalog.every_index_on(table).now().unwrap(),
            "indexes of {table:?}"
        );
        assert_eq!(
            catalog.fields_on(namespace, database, table).now().unwrap(),
            whole,
            "fields of {table:?}"
        );
        let present = catalog.table(table).now().unwrap().is_some();
        if !present && !(indexes.is_empty() && whole.is_empty()) {
            dropped_with_children += 1;
        }
        if present && !indexes.is_empty() {
            live_with_indexes.push(table.get().to_string());
        }
    }
    for namespace in catalog.namespaces().now().unwrap() {
        let mut whole = catalog
            .all(system::DATABASES, DatabaseDefinition::from_value)
            .now()
            .unwrap();
        whole.retain(|found| found.namespace == namespace.id);
        assert_eq!(
            catalog.databases_in(namespace.id).now().unwrap(),
            whole,
            "databases of {namespace:?}"
        );
        for database in whole {
            let mut tables = catalog
                .all(system::TABLES, TableDefinition::from_value)
                .now()
                .unwrap();
            tables.retain(|found| found.namespace == namespace.id && found.database == database.id);
            assert!(!tables.is_empty(), "the control: a database with tables");
            assert_eq!(
                catalog.tables_in(namespace.id, database.id).now().unwrap(),
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

/// A table's indexes and fields, held between statements once one reader has
/// read them (`rows`), are still what each reader's own snapshot holds: a
/// change committed elsewhere is seen by the next reader, a reader begun before
/// it keeps the old answer, a transaction sees its own definitions, and a
/// definition it cancelled is seen by nobody.
#[test]
fn a_held_definition_is_the_one_each_snapshot_holds() {
    use crate::{FieldShape, IndexShape, Store, TableShape};
    use std::sync::Arc;
    use tessari_types::{FieldKind, Path};

    let store = Store::open(Arc::new(tessari_kv::MemoryBackend::new()))
        .now()
        .unwrap();
    let mut transaction = store.begin().now().unwrap();
    let mut catalog = Catalog::new(&mut transaction);
    let namespace = catalog.create_namespace("prod").now().unwrap().id;
    let database = catalog.create_database(namespace, "shop").now().unwrap().id;
    let table = catalog
        .create_table(namespace, database, "users", TableShape::default())
        .now()
        .unwrap()
        .id;
    let declare = |catalog: &mut Catalog<'_, '_>, name: &str| {
        catalog
            .create_index(
                table,
                &format!("by_{name}"),
                vec![Path::field(name)],
                IndexShape::default(),
            )
            .now()
            .unwrap();
        catalog
            .create_field(table, name, FieldKind::Any, FieldShape::default())
            .now()
            .unwrap();
    };
    declare(&mut catalog, "a");
    transaction.commit().now().unwrap();
    let answer = |catalog: &Catalog<'_, '_>| {
        let indexes: Vec<String> = catalog
            .indexes_on(namespace, database, table)
            .now()
            .unwrap()
            .into_iter()
            .map(|index| index.name)
            .collect();
        let fields: Vec<String> = catalog
            .fields_on(namespace, database, table)
            .now()
            .unwrap()
            .into_iter()
            .map(|field| field.name)
            .collect();
        (indexes, fields)
    };
    let read = || {
        let mut transaction = store.begin().now().unwrap();
        answer(&Catalog::new(&mut transaction))
    };
    let named = |names: &[&str], prefix: &str| -> Vec<String> {
        names.iter().map(|name| format!("{prefix}{name}")).collect()
    };

    assert_eq!(read(), (named(&["a"], "by_"), named(&["a"], "")));
    // Read twice: the second is answered from what the first held.
    assert_eq!(read(), (named(&["a"], "by_"), named(&["a"], "")));
    let mut before = store.begin().now().unwrap();

    let mut transaction = store.begin().now().unwrap();
    declare(&mut Catalog::new(&mut transaction), "b");
    transaction.commit().now().unwrap();
    assert_eq!(
        read(),
        (named(&["a", "b"], "by_"), named(&["a", "b"], "")),
        "a definition committed elsewhere is not seen by the next reader"
    );
    assert_eq!(
        answer(&Catalog::new(&mut before)),
        (named(&["a"], "by_"), named(&["a"], "")),
        "a reader begun before the change is answered with what came after it"
    );

    let mut transaction = store.begin().now().unwrap();
    let mut catalog = Catalog::new(&mut transaction);
    declare(&mut catalog, "c");
    assert_eq!(
        answer(&catalog),
        (named(&["a", "b", "c"], "by_"), named(&["a", "b", "c"], "")),
        "a transaction does not see its own definitions"
    );
    drop(transaction);
    assert_eq!(
        read(),
        (named(&["a", "b"], "by_"), named(&["a", "b"], "")),
        "a cancelled definition is seen"
    );

    let mut transaction = store.begin().now().unwrap();
    let mut catalog = Catalog::new(&mut transaction);
    let first = catalog
        .indexes_on(namespace, database, table)
        .now()
        .unwrap()[0]
        .id;
    catalog.drop_index(first).now().unwrap();
    transaction.commit().now().unwrap();
    assert_eq!(
        read().0,
        named(&["b"], "by_"),
        "a dropped index is still found"
    );

    // A definition rewritten in place — `REBUILD INDEX` writes the index row
    // and claims no name — is seen by the next reader too.
    let unique_now = |store: &Store| {
        let mut transaction = store.begin().now().unwrap();
        Catalog::new(&mut transaction)
            .indexes_on(namespace, database, table)
            .now()
            .unwrap()[0]
            .unique
    };
    let held = unique_now(&store);
    let mut transaction = store.begin().now().unwrap();
    let mut catalog = Catalog::new(&mut transaction);
    let mut rewritten = catalog
        .indexes_on(namespace, database, table)
        .now()
        .unwrap()[0]
        .clone();
    rewritten.unique = !held;
    catalog.rebuild_index(&rewritten);
    transaction.commit().now().unwrap();
    assert_eq!(
        unique_now(&store),
        !held,
        "an index row rewritten without its name is answered as it was"
    );
}
