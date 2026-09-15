//! A ninth kind must not make an eighth-kind store unreadable.
//!
//! The same test S5.2 asked of queues, one kind further on, and the reason has
//! not changed: a definition built by the current code and read back by the
//! current code proves the two halves agree with each other, which they always
//! will. It says nothing about a store an earlier binary wrote.
//!
//! So the fixture below is bytes, checked in — a plain table's encoding, which
//! a view does not move because a view's read is written only when the kind is a
//! view. From here on the fixture is the record: a change that alters how a
//! plain table decodes fails this test rather than a store somewhere.
//!
//! # The downgrade this kind cannot make safe, stated in a test
//!
//! Reading forwards is fine and is asserted here. Reading a view **backwards**,
//! with a binary that predates the kind, is not: that build finds no view field
//! and reads the entry as a plain table over an empty prefix, so `SELECT`
//! answers nothing instead of the view's records. There is no encoding that
//! prevents it — an older reader cannot be taught about a field it has never
//! heard of — so it is documented rather than defended, and the last test here
//! pins the shape it takes so that nobody discovers it from a store.

#![allow(clippy::panic, clippy::unwrap_used)]

use tessari_encoding::{decode_payload, encode_payload};
use tessari_storage::{TableDefinition, TableKind, ViewDeclaration};
use tessari_types::{DatabaseId, IdentityKind, NamespaceId, TableId, Value};

/// One table definition, as a store written before views existed holds it.
///
/// `DEFINE TABLE jobs` in namespace 1, database 2, table id 7, schemaless, with
/// the default identity kind — the same fixture the queue kind was held to, for
/// the same reason: it is the encoding every release before this one produced.
const PRE_VIEW_TABLE: &str = "0d0000000a000000066275636b657403000000000a636f6c6c656374696f6e0300\
                              0000000864617461626173650401800000000000000200000004656467650300000\
                              0000367656f030000000002696404018000000000000007000000086964656e74697\
                              4790500000003696e74000000046e616d6505000000046a6f6273000000096e616d6\
                              57370616365040180000000000000010000000a736368656d6166756c6c0300";

fn bytes_of(hex: &str) -> Vec<u8> {
    let packed: Vec<char> = hex.chars().filter(|held| !held.is_whitespace()).collect();
    packed
        .chunks(2)
        .map(|pair| {
            let digits: String = pair.iter().collect();
            u8::from_str_radix(&digits, 16).unwrap()
        })
        .collect()
}

/// A view over `staff`, at the given id.
fn view(id: u32, read: &str) -> TableDefinition {
    TableDefinition {
        id: TableId::new(id),
        namespace: NamespaceId::new(1),
        database: DatabaseId::new(2),
        name: "engineers".to_owned(),
        schemafull: false,
        kind: TableKind::View(ViewDeclaration {
            read: read.to_owned(),
        }),
        identity: IdentityKind::default(),
        graph: None,
        conflict: None,
    }
}

#[test]
fn a_definition_written_before_views_existed_still_reads_as_a_plain_table() {
    let stored = decode_payload(&bytes_of(PRE_VIEW_TABLE)).unwrap();
    let definition = TableDefinition::from_value(&stored).unwrap();

    // The ninth variant adds a field an older definition simply lacks, which is
    // the path every kind since `Vector` has taken. Nothing is refused for being
    // silent about a kind that did not exist when it was written.
    assert_eq!(definition.kind, TableKind::Table);
    assert_eq!(definition.name, "jobs");
    assert_eq!(definition.id, TableId::new(7));
    assert!(!definition.schemafull);
    assert_eq!(definition.view_read(), None);
}

#[test]
fn a_view_round_trips_with_its_read_unchanged() {
    // Deliberately awkward text — two spaces, a trailing clause, a quoted
    // literal — because the claim is that the read comes back *as written* and
    // a tidy string would not tell a stored text from a re-rendered one.
    let written = "SELECT name,  team FROM staff WHERE team = 'eng' ORDER BY name";
    let definition = view(7, written);

    let bytes = encode_payload(&definition.to_value()).into_bytes();
    let read = TableDefinition::from_value(&decode_payload(&bytes).unwrap()).unwrap();

    assert_eq!(read.kind, definition.kind);
    assert_eq!(read.view_read(), Some(written));
}

#[test]
fn a_definition_claiming_to_be_a_view_and_an_edge_is_refused() {
    let Value::Object(mut fields) = view(9, "SELECT * FROM staff").to_value() else {
        panic!("a definition encodes as an object");
    };
    fields.insert("edge".to_owned(), Value::Bool(true));

    // Refused rather than resolved by precedence, the rule every kind before it
    // keeps: a stored table claiming two kinds is not a table this build can
    // serve correctly under either reading, and picking one would put the store
    // into the state the kind exists to make unrepresentable.
    assert!(TableDefinition::from_value(&Value::Object(fields)).is_err());
}

#[test]
fn an_older_reader_sees_a_view_as_an_empty_table_and_that_is_the_downgrade() {
    let Value::Object(mut fields) = view(11, "SELECT * FROM staff").to_value() else {
        panic!("a definition encodes as an object");
    };
    // What a build that predates views does: it has no name for this field, so
    // it never asks for it.
    fields.remove("view");
    let older = TableDefinition::from_value(&Value::Object(fields)).unwrap();

    // A plain table over a prefix nothing ever wrote to. `SELECT` against it
    // answers **nothing** — not a refusal, not the view's records — and `CREATE`
    // against it succeeds and writes records this build will never read. That is
    // the cost of opening a store holding views with an older binary, and it is
    // asserted here so that it is a documented property rather than a discovery.
    assert_eq!(older.kind, TableKind::Table);
    assert_eq!(older.view_read(), None);
    assert_eq!(older.name, "engineers");
}
