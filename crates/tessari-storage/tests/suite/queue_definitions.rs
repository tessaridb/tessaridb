//! An eighth kind must not make a seventh-kind store unreadable.
//!
//! Criterion S5.2 asks for the round trip to be proved against a **stored**
//! artifact rather than a freshly built one, and the distinction is the whole
//! point: a definition built by the current code and read back by the current
//! code proves the two halves agree with each other, which they always will. It
//! says nothing about a store somebody else's binary wrote.
//!
//! So the fixture below is bytes, checked in. They were produced by this
//! release's encoder for a plain table, which is the same encoding the release
//! **before** queues produced — a queue's declaration is written only when the
//! kind is a queue, so a plain table's bytes did not move. From here on the
//! fixture is the record: a change that alters how a plain table decodes fails
//! this test rather than a store somewhere.

#![allow(clippy::panic, clippy::unwrap_used)]

use tessari_encoding::{decode_payload, encode_payload};
use tessari_storage::{QueueDeclaration, TableDefinition, TableKind};
use tessari_types::{DatabaseId, Duration, IdentityKind, NamespaceId, TableId};

/// One table definition, as a store written before queues existed holds it.
///
/// `DEFINE TABLE jobs` in namespace 1, database 2, table id 7, schemaless, with
/// the default identity kind. Every flag the encoding carries is `false` and no
/// declaration is present — which is exactly what "written before the kind
/// existed" looks like on disk.
const PRE_QUEUE_TABLE: &str = "0d0000000a000000066275636b657403000000000a636f6c6c656374696f6e0300\
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

#[test]
fn a_definition_written_before_queues_existed_still_reads_as_a_plain_table() {
    let stored = decode_payload(&bytes_of(PRE_QUEUE_TABLE)).unwrap();
    let definition = TableDefinition::from_value(&stored).unwrap();

    // The eighth variant adds a field an older definition simply lacks, which is
    // the same path `Vault` and `Geo` took. Nothing here is refused for being
    // silent about a kind that did not exist when it was written.
    assert_eq!(definition.kind, TableKind::Table);
    assert_eq!(definition.name, "jobs");
    assert_eq!(definition.id, TableId::new(7));
    assert!(!definition.schemafull);
}

#[test]
fn a_queue_definition_round_trips_with_both_clauses() {
    let declared = QueueDeclaration {
        timeout: Duration::new(30, 0).unwrap(),
        attempts: Some(5),
    };
    let definition = TableDefinition {
        id: TableId::new(7),
        namespace: NamespaceId::new(1),
        database: DatabaseId::new(2),
        name: "jobs".to_owned(),
        schemafull: false,
        kind: TableKind::Queue(declared),
        identity: IdentityKind::default(),
        graph: None,
        conflict: None,
        shards: None,
    };

    let bytes = encode_payload(&definition.to_value()).into_bytes();
    let read = TableDefinition::from_value(&decode_payload(&bytes).unwrap()).unwrap();

    assert_eq!(read.kind, TableKind::Queue(declared));
}

#[test]
fn a_queue_declared_without_a_ceiling_reads_back_as_unlimited() {
    let declared = QueueDeclaration {
        timeout: Duration::new(5, 0).unwrap(),
        attempts: None,
    };
    let definition = TableDefinition {
        id: TableId::new(8),
        namespace: NamespaceId::new(1),
        database: DatabaseId::new(2),
        name: "mail".to_owned(),
        schemafull: false,
        kind: TableKind::Queue(declared),
        identity: IdentityKind::default(),
        graph: None,
        conflict: None,
        shards: None,
    };

    let bytes = encode_payload(&definition.to_value()).into_bytes();
    let read = TableDefinition::from_value(&decode_payload(&bytes).unwrap()).unwrap();

    // Absent rather than zero. Zero is the one number that would have to mean
    // "unlimited" while reading as "never hand this out", so it is not written
    // and the grammar refuses it.
    assert_eq!(read.kind, TableKind::Queue(declared));
}

#[test]
fn a_definition_claiming_to_be_a_queue_and_an_edge_is_refused() {
    let definition = TableDefinition {
        id: TableId::new(9),
        namespace: NamespaceId::new(1),
        database: DatabaseId::new(2),
        name: "both".to_owned(),
        schemafull: false,
        kind: TableKind::Queue(QueueDeclaration {
            timeout: Duration::new(30, 0).unwrap(),
            attempts: None,
        }),
        identity: IdentityKind::default(),
        graph: None,
        conflict: None,
        shards: None,
    };
    let tessari_types::Value::Object(mut fields) = definition.to_value() else {
        panic!("a definition encodes as an object");
    };
    fields.insert("edge".to_owned(), tessari_types::Value::Bool(true));

    // Refused rather than resolved by precedence: a stored table claiming two
    // kinds is not a table this build can serve correctly under either reading,
    // and picking one would put the store into the state the kind exists to make
    // unrepresentable.
    let refused = TableDefinition::from_value(&tessari_types::Value::Object(fields));
    assert!(refused.is_err());
}
