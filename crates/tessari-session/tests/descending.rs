//! A bounded descending read, taken from an index already in that order.
//!
//! `SELECT * FROM users ORDER BY joined DESC LIMIT 10` read every record,
//! evaluated the key on each, sorted all of them and threw away all but ten. The
//! index holds the same order — that is what it means for the sort order and the
//! index order to be one order — so the shape is a walk backwards that stops.
//!
//! What this file is mostly about is the **stopping**, because that is where an
//! ordering served by an index can answer differently from one applied to a
//! scan. Two rules the sort makes and the index does not:
//!
//! - a record whose key is absent still sorts somewhere (below every value), and
//!   it has **no index entry** at all;
//! - ties break by identity **ascending**, while a key is stored as
//!   `value ++ identity`, so walking backwards yields a tie group descending.
//!
//! The first is why this is descending — absences come last, so a bound reaches
//! them only when the index has run out. The second is why the walk drains the
//! group straddling the bound before anything is cut.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{AccessPath, Outcome, Session};
use tessari_storage::Store;
use tessari_types::{RecordId, Value};

fn store() -> Store {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    Store::open(backend).unwrap()
}

const INDEX: &str = "DEFINE INDEX by_joined ON users FIELDS joined;";

/// Eight people with distinct joining years, written out of order so that
/// identity order and value order are not the same order.
fn ready(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE TABLE users;\n\
             CREATE users:1 = { name: 'ada', joined: 1843 };\n\
             CREATE users:2 = { name: 'grace', joined: 1952 };\n\
             CREATE users:3 = { name: 'alan', joined: 1936 };\n\
             CREATE users:4 = { name: 'katherine', joined: 1953 };\n\
             CREATE users:5 = { name: 'edsger', joined: 1968 };\n\
             CREATE users:6 = { name: 'barbara', joined: 1961 };\n\
             CREATE users:7 = { name: 'donald', joined: 1962 };\n\
             CREATE users:8 = { name: 'margaret', joined: 1969 };",
        )
        .unwrap();
    session
}

/// The identities a read answers with, **in the order it answered**.
///
/// Not sorted by the test: the order is what is under test, and a test that
/// sorted the answer could not see the one thing this file exists to check.
fn ids(session: &mut Session<'_>, read: &str) -> Vec<RecordId> {
    session
        .run(read)
        .unwrap()
        .last()
        .unwrap()
        .records()
        .unwrap()
        .iter()
        .map(|(id, _)| id.clone())
        .collect()
}

fn path(session: &mut Session<'_>, read: &str) -> AccessPath {
    let outcomes = session.run(read).unwrap();
    let Some(Outcome::Records { path, .. }) = outcomes.last() else {
        panic!("a read answered with {:?}", outcomes.last());
    };
    *path
}

fn plan(session: &mut Session<'_>, read: &str, field: &str) -> String {
    let outcomes = session.run(&format!("EXPLAIN {read}")).unwrap();
    let Some(Outcome::Value(Value::Object(fields))) = outcomes.last() else {
        panic!("a plan answered with {:?}", outcomes.last());
    };
    format!("{:?}", fields.get(field).unwrap_or(&Value::None))
}

const READ: &str = "SELECT * FROM users ORDER BY joined DESC LIMIT 3;";

#[test]
fn a_bounded_descending_read_is_taken_from_the_index() {
    let store = store();
    let mut session = ready(&store);
    assert_eq!(path(&mut session, READ), AccessPath::Scan);
    session.run(INDEX).unwrap();
    assert_eq!(path(&mut session, READ), AccessPath::Ordered);
    assert_eq!(plan(&mut session, READ, "access"), r#"String("ordered")"#);
    assert_eq!(plan(&mut session, READ, "index"), r#"String("by_joined")"#);
}

#[test]
fn the_answers_are_the_ones_a_scan_gives() {
    // The rule the whole wave could have broken, asserted over every shape the
    // bound can take: an index changes what a read costs and never what it
    // answers — including the order it answers in.
    let reads = [
        READ,
        "SELECT * FROM users ORDER BY joined DESC LIMIT 1;",
        "SELECT * FROM users ORDER BY joined DESC LIMIT 8;",
        // A bound past the end of the table.
        "SELECT * FROM users ORDER BY joined DESC LIMIT 50;",
        "SELECT * FROM users ORDER BY joined DESC START 2 LIMIT 3;",
        "SELECT * FROM users ORDER BY joined DESC START 7 LIMIT 5;",
        // A `START` past the end answers with nothing, from either path.
        "SELECT * FROM users ORDER BY joined DESC START 40 LIMIT 5;",
    ];
    let unindexed = store();
    let mut without = ready(&unindexed);
    let indexed = store();
    let mut with = ready(&indexed);
    with.run(INDEX).unwrap();

    for read in reads {
        assert_eq!(ids(&mut with, read), ids(&mut without, read), "{read}");
    }
    // …and the expected answer is written out once, so the equality above is not
    // two wrong answers agreeing with each other.
    assert_eq!(
        ids(&mut with, READ),
        vec![RecordId::Int(8), RecordId::Int(5), RecordId::Int(7)]
    );
    // `START 2` passes over 1969 and 1968, so the page begins at 1962.
    assert_eq!(
        ids(&mut with, reads[4]),
        vec![RecordId::Int(7), RecordId::Int(6), RecordId::Int(4)]
    );
}

#[test]
fn a_tie_group_straddling_the_bound_resolves_by_identity_ascending() {
    // The trap. A key is stored as `value ++ identity`, so walking backwards
    // gives a tie group with its identities **descending** — while the order
    // this store answers in breaks ties by identity ascending, so that adding an
    // index cannot reorder equal rows. Cutting the walk at the bound would take
    // the three largest identities where the answer wants the three smallest.
    let script = "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
                  DEFINE DATABASE shop; USE DATABASE shop;\n\
                  DEFINE TABLE users;\n\
                  CREATE users:1 = { joined: 1900 };\n\
                  CREATE users:2 = { joined: 1900 };\n\
                  CREATE users:3 = { joined: 1900 };\n\
                  CREATE users:4 = { joined: 1900 };\n\
                  CREATE users:5 = { joined: 1900 };\n\
                  CREATE users:6 = { joined: 2000 };";
    let unindexed = store();
    let mut without = Session::new(&unindexed);
    without.run(script).unwrap();
    let indexed = store();
    let mut with = Session::new(&indexed);
    with.run(script).unwrap();
    with.run(INDEX).unwrap();

    let read = "SELECT * FROM users ORDER BY joined DESC LIMIT 3;";
    assert_eq!(path(&mut with, read), AccessPath::Ordered);
    assert_eq!(ids(&mut with, read), ids(&mut without, read));
    assert_eq!(
        ids(&mut with, read),
        vec![RecordId::Int(6), RecordId::Int(1), RecordId::Int(2)],
        "the newest, then the two smallest identities of the tie group"
    );
    // The whole group, ascending within it, when the bound reaches past it.
    assert_eq!(
        ids(
            &mut with,
            "SELECT * FROM users ORDER BY joined DESC LIMIT 6;"
        ),
        vec![
            RecordId::Int(6),
            RecordId::Int(1),
            RecordId::Int(2),
            RecordId::Int(3),
            RecordId::Int(4),
            RecordId::Int(5)
        ]
    );
}

#[test]
fn a_record_with_no_value_sorts_last_and_the_index_gives_the_read_up() {
    // The reason this node is descending. A record whose key is absent has no
    // index entry, and the sort still places it — below every value. Descending
    // that is the end of the answer, so the index serves the bound it can fill
    // and hands back a read whose bound it cannot.
    let script = "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
                  DEFINE DATABASE shop; USE DATABASE shop;\n\
                  DEFINE TABLE users;\n\
                  CREATE users:1 = { joined: 1990 };\n\
                  CREATE users:2 = { name: 'nobody' };\n\
                  CREATE users:3 = { joined: 1980 };\n\
                  CREATE users:4 = { name: 'also nobody' };";
    let unindexed = store();
    let mut without = Session::new(&unindexed);
    without.run(script).unwrap();
    let indexed = store();
    let mut with = Session::new(&indexed);
    with.run(script).unwrap();
    with.run(INDEX).unwrap();

    // Two entries, and a bound of two: the index fills it.
    let short = "SELECT * FROM users ORDER BY joined DESC LIMIT 2;";
    assert_eq!(path(&mut with, short), AccessPath::Ordered);
    assert_eq!(ids(&mut with, short), ids(&mut without, short));
    assert_eq!(
        ids(&mut with, short),
        vec![RecordId::Int(1), RecordId::Int(3)]
    );

    // A bound of three needs a record the index does not hold, so the read
    // reports the scan that answered it — and answers the same as one.
    let long = "SELECT * FROM users ORDER BY joined DESC LIMIT 3;";
    assert_eq!(ids(&mut with, long), ids(&mut without, long));
    assert_eq!(
        ids(&mut with, long),
        vec![RecordId::Int(1), RecordId::Int(3), RecordId::Int(2)],
        "the absences come last, in identity order"
    );
    assert_eq!(path(&mut with, long), AccessPath::Scan);
}

#[test]
fn every_other_shape_is_a_scan_and_answers_the_same() {
    // Each is a way the order the index holds could differ from the order the
    // read must answer in, and each is refused by name rather than guessed at.
    let refused = [
        // Ascending: the records with no value sort **first**, and those are
        // exactly the ones the index does not hold.
        "SELECT * FROM users ORDER BY joined LIMIT 3;",
        // A second key orders records the index never separated.
        "SELECT * FROM users ORDER BY joined DESC, name DESC LIMIT 3;",
        // A computed key is not what any index holds.
        "SELECT * FROM users ORDER BY joined + 1 DESC LIMIT 3;",
        // No bound: the read wants every record, so there is nothing to stop.
        "SELECT * FROM users ORDER BY joined DESC;",
        // Grouping folds the records the order would have chosen between.
        "SELECT joined, count(*) AS n FROM users GROUP BY joined ORDER BY joined DESC LIMIT 3;",
        // The sort runs after the projection and may name what it produced.
        "SELECT joined AS year FROM users ORDER BY year DESC LIMIT 3;",
        // A field the index does not hold at all.
        "SELECT * FROM users ORDER BY name DESC LIMIT 3;",
    ];
    let unindexed = store();
    let mut without = ready(&unindexed);
    let indexed = store();
    let mut with = ready(&indexed);
    with.run(INDEX).unwrap();

    for read in refused {
        assert_ne!(path(&mut with, read), AccessPath::Ordered, "{read}");
        assert_eq!(ids(&mut with, read), ids(&mut without, read), "{read}");
    }
}

#[test]
fn a_composite_index_serves_an_order_on_its_leading_field_and_not_on_a_later_one() {
    // This test used to assert the opposite, and its reason was that the tie
    // group at the bound is a group of leading values "which cannot be read off
    // a key, because the encoding normalises and is not reversible". The premise
    // is true and the conclusion did not follow: a tie test asks whether two
    // entries *agree*, never what they hold, and agreement is byte equality over
    // a self-delimiting prefix. Kept as one test rather than split, because the
    // two halves are the same rule seen from its two sides.
    let store = store();
    let mut session = ready(&store);
    session
        .run("DEFINE INDEX by_joined_name ON users FIELDS joined, name;")
        .unwrap();

    // `joined` is the leading field, so the entries are stored in exactly the
    // order this read asks for.
    assert_eq!(path(&mut session, READ), AccessPath::Ordered);
    assert_eq!(plan(&mut session, READ, "access"), r#"String("ordered")"#);

    // `name` is not. Its entries are grouped inside each `joined`, so reading
    // them in key order yields `name` restarted once per `joined` — not that
    // field's order at any point, and not nearly it either.
    let later = "SELECT * FROM users ORDER BY name DESC LIMIT 3;";
    assert_eq!(path(&mut session, later), AccessPath::Scan);
    assert_eq!(plan(&mut session, later, "access"), r#"String("scan")"#);
}

#[test]
fn a_search_index_does_not_serve_an_order() {
    // It holds terms, not values, and a term's order is not the field's.
    let store = store();
    let mut session = ready(&store);
    session
        .run("DEFINE INDEX by_name ON users FIELDS name SEARCH;")
        .unwrap();
    let read = "SELECT * FROM users ORDER BY name DESC LIMIT 3;";
    assert_eq!(path(&mut session, read), AccessPath::Scan);
}

#[test]
fn the_order_does_not_disclose_a_field_the_caller_cannot_read() {
    // A field permission removes the field **before** anything looks at the
    // record, so a caller without it sorts by `none` and gets identity order. An
    // order taken from the index would sort by the values themselves — the
    // ordering disclosing what the projection hides, one comparison at a time.
    const PASSWORD: &str = "correct horse battery";
    let store = store();
    let mut opening = Session::new(&store);
    opening
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE TABLE staff;\n\
             CREATE staff:1 = { name: 'ada', salary: 10 };\n\
             CREATE staff:2 = { name: 'grace', salary: 30 };\n\
             CREATE staff:3 = { name: 'alan', salary: 20 };\n\
             DEFINE INDEX by_salary ON staff FIELDS salary;\n\
             DEFINE USER root ROLE owner PASSWORD 'correct horse battery';",
        )
        .unwrap();
    let mut root = Session::new(&store);
    root.sign_in("root", PASSWORD).unwrap();
    root.run(
        "USE NAMESPACE prod; USE DATABASE shop;\n\
              DEFINE USER ada ON prod.shop ROLE editor PASSWORD 'correct horse battery';\n\
              GRANT read ON staff FIELDS name TO ada;",
    )
    .unwrap();

    let mut ada = Session::new(&store);
    ada.sign_in("ada", PASSWORD).unwrap();
    ada.run("USE NAMESPACE prod; USE DATABASE shop;").unwrap();

    let read = "SELECT * FROM staff ORDER BY salary DESC LIMIT 2;";
    assert_eq!(
        ids(&mut ada, read),
        vec![RecordId::Int(1), RecordId::Int(2)],
        "identity order, because every key is a field this caller cannot see"
    );
    assert_eq!(path(&mut ada, read), AccessPath::Scan);
    // The owner, who can see it, gets the order the values have.
    assert_eq!(path(&mut root, read), AccessPath::Ordered);
    assert_eq!(
        ids(&mut root, read),
        vec![RecordId::Int(2), RecordId::Int(3)]
    );
}

#[test]
fn a_write_in_the_same_transaction_gives_the_read_up() {
    // Entries are derived at commit, so a record this transaction wrote has none
    // and the index cannot place it. The read falls back and the uncommitted
    // record is placed by the sort, which needs no index to know where it goes.
    let store = store();
    let mut session = ready(&store);
    session.run(INDEX).unwrap();
    let outcomes = session
        .run(
            "BEGIN;\n\
             CREATE users:9 = { name: 'later', joined: 2020 };\n\
             SELECT * FROM users ORDER BY joined DESC LIMIT 2;\n\
             COMMIT;",
        )
        .unwrap();
    let Some(Outcome::Records {
        records,
        path: took,
    }) = outcomes.get(2)
    else {
        panic!("the read answered with {:?}", outcomes.get(2));
    };
    assert_eq!(*took, AccessPath::Scan);
    assert_eq!(
        records.iter().map(|(id, _)| id.clone()).collect::<Vec<_>>(),
        vec![RecordId::Int(9), RecordId::Int(8)]
    );
    // And once it has committed, the index holds it and the order is served.
    let after = "SELECT * FROM users ORDER BY joined DESC LIMIT 2;";
    assert_eq!(path(&mut session, after), AccessPath::Ordered);
    assert_eq!(
        ids(&mut session, after),
        vec![RecordId::Int(9), RecordId::Int(8)],
        "the same answer the scan gave a moment ago"
    );
}
