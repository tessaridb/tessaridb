use super::*;

/// The bytes a namespace held before the replication clause existed.
///
/// Pinned as a literal rather than produced by an encoder, so that this is
/// genuinely a stored value from an older build and not a round trip of
/// today's. A namespace written then must read as **never stated** — which
/// is the true reading of a store that had no way to say anything, not a
/// fallback — and must encode back to exactly the same object, so nothing
/// already on disk is rewritten by being read.
#[test]
fn a_namespace_stored_before_the_clause_reads_as_never_stated() {
    let stored = Value::Object(BTreeMap::from([
        ("id".to_owned(), number(7)),
        ("name".to_owned(), Value::from("prod")),
    ]));
    let read = NamespaceDefinition::from_value(&stored).unwrap();
    assert_eq!(read.replication, None);
    assert_eq!(read.to_value(), stored, "a read must not rewrite it");
}

#[test]
fn a_stated_class_round_trips_and_a_definition_without_one_reads_as_silence() {
    // G027 S2.1. The second half is the one that matters on disk: a
    // namespace written before this field existed must still decode, and
    // must decode as *never stated* rather than as either answer — the same
    // property the replication clause bought, and the same reason nothing
    // stored is rewritten.
    for class in [
        ReplicationClass::SingleLeader,
        ReplicationClass::MultiMaster,
    ] {
        let namespace = NamespaceDefinition {
            id: NamespaceId::new(7),
            name: "prod".to_owned(),
            replication: None,
            class: Some(class),
            acknowledge: None,
        };
        let read = NamespaceDefinition::from_value(&namespace.to_value()).unwrap();
        assert_eq!(read, namespace, "{class}");
    }

    // Pinned as a literal for its neighbour's reason: this is a namespace a
    // build without the class field wrote, not a round trip of today's, and
    // it must read as never stated and encode back unchanged.
    let stored = Value::Object(BTreeMap::from([
        ("id".to_owned(), number(7)),
        ("name".to_owned(), Value::from("prod")),
    ]));
    let read = NamespaceDefinition::from_value(&stored).unwrap();
    assert_eq!(read.class, None);
    assert_eq!(read.to_value(), stored, "a read must not rewrite it");
}

#[test]
fn a_stated_policy_round_trips_and_is_not_silence() {
    for policy in [
        Replication::None,
        Replication::Factor(core::num::NonZeroU32::new(3).unwrap()),
    ] {
        let namespace = NamespaceDefinition {
            id: NamespaceId::new(7),
            name: "prod".to_owned(),
            replication: Some(policy),
            class: None,
            acknowledge: None,
        };
        let read = NamespaceDefinition::from_value(&namespace.to_value()).unwrap();
        assert_eq!(read, namespace, "{policy}");
        assert_ne!(read.replication, None, "{policy}");
    }
}

/// A policy a later build understands and this one does not refuses rather
/// than reading as silence — the same refusal a table's unknown identity
/// scheme takes, and for the same reason: serving a namespace as *nobody
/// ever declared one* when somebody did is the wrong answer given
/// confidently.
#[test]
fn a_policy_this_build_does_not_know_refuses() {
    let stored = Value::Object(BTreeMap::from([
        ("id".to_owned(), number(7)),
        ("name".to_owned(), Value::from("prod")),
        ("replication".to_owned(), Value::from("every-rack")),
    ]));
    let error = NamespaceDefinition::from_value(&stored).unwrap_err();
    assert!(
        matches!(
            error,
            Error::CatalogMalformed {
                field: "replication",
                ..
            }
        ),
        "{error}"
    );
}

#[test]
fn every_definition_round_trips_through_its_value() {
    let namespace = NamespaceDefinition {
        id: NamespaceId::new(7),
        name: "prod".to_owned(),
        replication: None,
        class: None,
        acknowledge: None,
    };
    assert_eq!(
        NamespaceDefinition::from_value(&namespace.to_value()).unwrap(),
        namespace
    );

    let database = DatabaseDefinition {
        id: DatabaseId::new(3),
        namespace: NamespaceId::new(7),
        name: "orders".to_owned(),
        params: std::collections::BTreeMap::new(),
    };
    assert_eq!(
        DatabaseDefinition::from_value(&database.to_value()).unwrap(),
        database
    );

    let table = TableDefinition {
        id: TableId::new(11),
        namespace: NamespaceId::new(7),
        database: DatabaseId::new(3),
        name: "line_items".to_owned(),
        schemafull: true,
        graph: Some(GraphId::new(9)),
        // Deliberately not the default here either, for the same reason the
        // identity below is not: a kind that round trips through three flags
        // is only proven by a kind that is not the one an absent flag gives.
        // The ceiling is present for that same reason once more — an absent
        // one round trips through a field that was never written, which
        // proves the default rather than the encoding.
        kind: TableKind::Bucket(Some(5 * 1024 * 1024)),
        // Deliberately not the default: a field that never travels round
        // trips perfectly as long as both ends agree on what it is when
        // absent, which is exactly the bug this assertion is for.
        identity: IdentityKind::Uuid,
        conflict: None,
        shards: None,
        partition: None,
        spread: false,
        auto_split: None,
        expire: None,
        // Not the default either, for the identity's reason: an empty list
        // round trips through a field that was never written.
        events: vec![super::super::EventDeclaration {
            name: "audit".to_owned(),
            on: vec![tessari_types::WriteKind::Update],
            when: Some("$after.v > 1".to_owned()),
            body: "CREATE log = { v: $after.v }".to_owned(),
        }],
    };
    assert_eq!(
        TableDefinition::from_value(&table.to_value()).unwrap(),
        table
    );
}
