#![allow(clippy::unwrap_used)]

use super::*;
use tessari_types::{GraphId, IdentityKind, IndexId, Path, TableId};

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
        // Not the default either, for the identity's reason: an empty list
        // round trips through a field that was never written.
        events: vec![super::EventDeclaration {
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

#[test]
fn a_table_entry_written_before_identities_were_declared_names_records_with_a_counter() {
    // Every table already in a store predates the field, and each one is
    // already naming records with a counter. Reading them as anything else
    // would rename what the *next* record in them is called.
    let fields = BTreeMap::from([
        (FIELD_ID.to_owned(), number(11)),
        (FIELD_NAMESPACE.to_owned(), number(7)),
        (FIELD_DATABASE.to_owned(), number(3)),
        (FIELD_NAME.to_owned(), Value::from("line_items")),
    ]);
    let read = TableDefinition::from_value(&Value::Object(fields)).unwrap();
    assert_eq!(read.identity, IdentityKind::Int);
}

#[test]
fn a_naming_scheme_this_build_does_not_know_is_refused_rather_than_read_as_the_default() {
    // The asymmetry with the test above is the whole point. *Absent* means a
    // store written before the field existed, and its answer is knowable.
    // *Present and unrecognised* means a store written by a later build, and
    // the one thing that must not happen is this build deciding the table
    // uses the scheme it happens to prefer and minting ids under it.
    let fields = BTreeMap::from([
        (FIELD_ID.to_owned(), number(11)),
        (FIELD_NAMESPACE.to_owned(), number(7)),
        (FIELD_DATABASE.to_owned(), number(3)),
        (FIELD_NAME.to_owned(), Value::from("line_items")),
        (FIELD_IDENTITY.to_owned(), Value::from("ulid")),
    ]);
    let error = TableDefinition::from_value(&Value::Object(fields)).unwrap_err();
    assert_eq!(error.code(), "corruption");

    // And a scheme that is not even a word is refused for the same reason
    // rather than falling through a `match` on the string.
    let fields = BTreeMap::from([
        (FIELD_ID.to_owned(), number(11)),
        (FIELD_NAMESPACE.to_owned(), number(7)),
        (FIELD_DATABASE.to_owned(), number(3)),
        (FIELD_NAME.to_owned(), Value::from("line_items")),
        (FIELD_IDENTITY.to_owned(), Value::Bool(true)),
    ]);
    let error = TableDefinition::from_value(&Value::Object(fields)).unwrap_err();
    assert_eq!(error.code(), "corruption");
}

#[test]
fn a_table_entry_written_before_schemas_existed_reads_as_schemaless() {
    let fields = BTreeMap::from([
        (FIELD_ID.to_owned(), number(11)),
        (FIELD_NAMESPACE.to_owned(), number(7)),
        (FIELD_DATABASE.to_owned(), number(3)),
        (FIELD_NAME.to_owned(), Value::from("line_items")),
    ]);
    let read = TableDefinition::from_value(&Value::Object(fields)).unwrap();
    assert!(!read.schemafull);
}

#[test]
fn a_schemafull_flag_that_is_not_a_boolean_is_refused_rather_than_read_as_false() {
    let fields = BTreeMap::from([
        (FIELD_ID.to_owned(), number(11)),
        (FIELD_NAMESPACE.to_owned(), number(7)),
        (FIELD_DATABASE.to_owned(), number(3)),
        (FIELD_NAME.to_owned(), Value::from("line_items")),
        (FIELD_SCHEMAFULL.to_owned(), Value::from("yes")),
    ]);
    let error = TableDefinition::from_value(&Value::Object(fields)).unwrap_err();
    assert_eq!(error.code(), "corruption");
}

#[test]
fn a_stored_table_claiming_two_kinds_is_refused_rather_than_read_as_one_of_them() {
    // In memory a table has one kind and a second one cannot be spelled. On
    // disk it is still three separate booleans, so the pair *is* writable —
    // by a build that predates the kind, or by corruption — and the decoder
    // is the only place left that can refuse it. Picking a winner here would
    // be the worse failure: the table would read as an edge on one replica
    // and a bucket on another, from bytes both agree on.
    let fields = BTreeMap::from([
        (FIELD_ID.to_owned(), number(11)),
        (FIELD_NAMESPACE.to_owned(), number(7)),
        (FIELD_DATABASE.to_owned(), number(3)),
        (FIELD_NAME.to_owned(), Value::from("line_items")),
        (FIELD_EDGE.to_owned(), Value::Bool(true)),
        (FIELD_BUCKET.to_owned(), Value::Bool(true)),
    ]);
    let error = TableDefinition::from_value(&Value::Object(fields)).unwrap_err();
    assert_eq!(error.code(), "corruption");
    let text = error.to_string();
    assert!(text.contains("kind"), "{text}");
}

#[test]
fn an_edge_table_round_trips_its_endpoints_and_the_order_its_edges_are_held_in() {
    let table = TableDefinition {
        id: TableId::new(11),
        namespace: NamespaceId::new(7),
        database: DatabaseId::new(3),
        name: "follows".to_owned(),
        schemafull: false,
        graph: None,
        shards: None,
        partition: None,
        spread: false,
        auto_split: None,
        events: Vec::new(),
        kind: TableKind::Edge(Some(EdgeDeclaration {
            from: TableId::new(4),
            to: TableId::new(5),
            // Deliberately descending, and deliberately not the same table at
            // both ends: an order that round trips as `false` and endpoints
            // that round trip transposed both survive an assertion made with
            // the defaults.
            order: Some(EdgeOrder {
                field: "at".to_owned(),
                descending: true,
            }),
        })),
        identity: IdentityKind::Int,
        conflict: None,
    };
    assert_eq!(
        TableDefinition::from_value(&table.to_value()).unwrap(),
        table
    );

    // And a declared pair with no declared order is a different value, not a
    // missing one — it reads back unordered rather than as the default order.
    let unordered = TableDefinition {
        kind: TableKind::Edge(Some(EdgeDeclaration {
            from: TableId::new(4),
            to: TableId::new(5),
            order: None,
        })),
        ..table.clone()
    };
    assert_eq!(
        TableDefinition::from_value(&unordered.to_value()).unwrap(),
        unordered
    );

    // And the bare `EDGE`, which is what every edge table declared before the
    // clause existed is: no endpoints field is written, and the entry reads
    // back permissive rather than as a pair nobody declared.
    let permissive = TableDefinition {
        kind: TableKind::Edge(None),
        ..table
    };
    assert_eq!(
        TableDefinition::from_value(&permissive.to_value()).unwrap(),
        permissive
    );
}

#[test]
fn a_vector_store_round_trips_its_width_and_its_distance() {
    let table = TableDefinition {
        id: TableId::new(11),
        namespace: NamespaceId::new(7),
        database: DatabaseId::new(3),
        name: "embeddings".to_owned(),
        schemafull: false,
        graph: None,
        shards: None,
        partition: None,
        spread: false,
        auto_split: None,
        events: Vec::new(),
        // Deliberately the second distance rather than the first: a store
        // that round tripped as `cosine` whatever it was declared with
        // survives an assertion made with the default.
        kind: TableKind::Vector(VectorDeclaration {
            dimension: 768,
            distance: VectorDistance::Euclidean,
        }),
        identity: IdentityKind::Int,
        conflict: None,
    };
    assert_eq!(
        TableDefinition::from_value(&table.to_value()).unwrap(),
        table
    );
}

#[test]
fn a_stored_vector_store_naming_a_distance_this_build_has_not_is_refused() {
    // Refused rather than read as `cosine`, on the reasoning `identity_kind`
    // records for an unknown word: the entry describes a store already
    // searched some other way, and answering its nearest-neighbour question
    // from a graph built for a different geometry returns plausible
    // neighbours that are not the nearest — with nothing in an error state.
    let fields = BTreeMap::from([
        (FIELD_ID.to_owned(), number(11)),
        (FIELD_NAMESPACE.to_owned(), number(7)),
        (FIELD_DATABASE.to_owned(), number(3)),
        (FIELD_NAME.to_owned(), Value::from("embeddings")),
        (
            FIELD_VECTOR.to_owned(),
            Value::Object(BTreeMap::from([
                (FIELD_DIMENSION.to_owned(), number(768)),
                (FIELD_DISTANCE.to_owned(), Value::from("manhattan")),
            ])),
        ),
    ]);
    let error = TableDefinition::from_value(&Value::Object(fields)).unwrap_err();
    assert_eq!(error.code(), "corruption");
}

#[test]
fn a_stored_vector_store_that_also_claims_a_flag_is_refused() {
    // The same refusal the three flags already get, extended to the kind
    // that is carried by a declaration instead of by a flag: on disk both
    // are writable side by side, and picking a winner would make one replica
    // read a bucket where another reads a vector store, from bytes they
    // agree on.
    let fields = BTreeMap::from([
        (FIELD_ID.to_owned(), number(11)),
        (FIELD_NAMESPACE.to_owned(), number(7)),
        (FIELD_DATABASE.to_owned(), number(3)),
        (FIELD_NAME.to_owned(), Value::from("embeddings")),
        (FIELD_BUCKET.to_owned(), Value::Bool(true)),
        (
            FIELD_VECTOR.to_owned(),
            Value::Object(BTreeMap::from([
                (FIELD_DIMENSION.to_owned(), number(768)),
                (FIELD_DISTANCE.to_owned(), Value::from("cosine")),
            ])),
        ),
    ]);
    let error = TableDefinition::from_value(&Value::Object(fields)).unwrap_err();
    assert_eq!(error.code(), "corruption");
    let text = error.to_string();
    assert!(text.contains("kind"), "{text}");
}

#[test]
fn stored_endpoints_with_a_direction_but_no_field_are_refused_rather_than_read_as_unordered() {
    // The order is the endpoint index's key suffix, so reading a half-written
    // order as "no order" would answer a bounded neighbour read from an index
    // that does not hold the order it is being asked for — in the right
    // sequence often enough, by accident, to look correct.
    let endpoints = BTreeMap::from([
        (FIELD_FROM.to_owned(), number(4)),
        (FIELD_TO.to_owned(), number(5)),
        (FIELD_DESCENDING.to_owned(), Value::Bool(true)),
    ]);
    let fields = BTreeMap::from([
        (FIELD_ID.to_owned(), number(11)),
        (FIELD_NAMESPACE.to_owned(), number(7)),
        (FIELD_DATABASE.to_owned(), number(3)),
        (FIELD_NAME.to_owned(), Value::from("follows")),
        (FIELD_EDGE.to_owned(), Value::Bool(true)),
        (FIELD_ENDPOINTS.to_owned(), Value::Object(endpoints)),
    ]);
    let error = TableDefinition::from_value(&Value::Object(fields)).unwrap_err();
    assert_eq!(error.code(), "corruption");
    let text = error.to_string();
    assert!(text.contains("order"), "{text}");
}

#[test]
fn a_stored_pair_on_a_table_that_is_not_an_edge_table_is_refused() {
    // The pair only means anything on an edge table, and this is the state
    // the kind exists to keep unrepresentable in memory — but disk is still
    // writable by an older build or by corruption. Reading it as a plain
    // table would silently discard a declared refusal, and reading it as an
    // edge table would give one replica a declared pair where another has
    // a table, from bytes both agree on.
    let endpoints = BTreeMap::from([
        (FIELD_FROM.to_owned(), number(4)),
        (FIELD_TO.to_owned(), number(5)),
    ]);
    let fields = BTreeMap::from([
        (FIELD_ID.to_owned(), number(11)),
        (FIELD_NAMESPACE.to_owned(), number(7)),
        (FIELD_DATABASE.to_owned(), number(3)),
        (FIELD_NAME.to_owned(), Value::from("follows")),
        (FIELD_EDGE.to_owned(), Value::Bool(false)),
        (FIELD_ENDPOINTS.to_owned(), Value::Object(endpoints)),
    ]);
    let error = TableDefinition::from_value(&Value::Object(fields)).unwrap_err();
    assert_eq!(error.code(), "corruption");
}

#[test]
fn a_definition_missing_a_field_is_refused_and_names_it() {
    let fields = BTreeMap::from([(FIELD_ID.to_owned(), number(1))]);
    let error = TableDefinition::from_value(&Value::Object(fields)).unwrap_err();
    let text = error.to_string();
    assert!(text.contains("namespace"), "{text}");
}

#[test]
fn an_index_definition_round_trips_a_route_through_the_text_it_is_stored_as() {
    // The catalog stores a path as its spelling, so a path that read back as
    // a different route would index one value and filter another — and both
    // sides would look right in isolation.
    let index = IndexDefinition {
        id: IndexId::new(2),
        namespace: NamespaceId::new(7),
        database: DatabaseId::new(3),
        table: TableId::new(11),
        name: "by_home_city".to_owned(),
        fields: vec![
            Path::parse("address.city").expect("a path"),
            Path::parse("tags[0].name").expect("a path"),
            Path::field("email"),
        ],
        unique: false,
        search: false,
        spatial: false,
        quantized: false,
        vector: None,
        costs: crate::catalog::SearchCosts::default(),
        engine: None,
        tokenizer: None,
    };
    assert_eq!(
        IndexDefinition::from_value(&index.to_value()).unwrap(),
        index
    );
}

#[test]
fn a_search_index_keeps_the_tokenizer_generation_it_was_built_by() {
    let mut index = IndexDefinition {
        id: IndexId::new(4),
        namespace: NamespaceId::new(7),
        database: DatabaseId::new(3),
        table: TableId::new(11),
        name: "by_body".to_owned(),
        fields: vec![Path::field("body")],
        unique: false,
        search: true,
        spatial: false,
        quantized: false,
        vector: None,
        costs: crate::catalog::SearchCosts::default(),
        engine: None,
        tokenizer: Some(tessari_types::TOKENIZER_GENERATION),
    };
    let read = IndexDefinition::from_value(&index.to_value()).unwrap();
    assert_eq!(read, index);
    assert!(!read.needs_rebuild());
    // An index written before the generation was recorded: no field at all.
    index.tokenizer = None;
    let stored = index.to_value();
    assert!(
        matches!(&stored, Value::Object(fields) if !fields.contains_key(FIELD_TOKENIZER)),
        "nothing written when unrecorded: {stored:?}"
    );
    let legacy = IndexDefinition::from_value(&stored).unwrap();
    assert_eq!(legacy.tokenizer, None);
    assert!(
        legacy.needs_rebuild(),
        "an unrecorded term index is not known current"
    );
    // An ordered index holds no terms and never needs one.
    index.search = false;
    assert!(!index.needs_rebuild());
}

#[test]
fn an_index_entry_written_before_paths_existed_reads_as_a_single_field() {
    // Every definition on disk today spells one plain field name, and a plain
    // field name is a path of one step. Nothing has to be migrated.
    let fields = BTreeMap::from([
        (FIELD_ID.to_owned(), number(2)),
        (FIELD_NAMESPACE.to_owned(), number(7)),
        (FIELD_DATABASE.to_owned(), number(3)),
        (FIELD_TABLE.to_owned(), number(11)),
        (FIELD_NAME.to_owned(), Value::from("by_email")),
        (
            FIELD_FIELDS.to_owned(),
            Value::Array(vec![Value::from("email")]),
        ),
        (FIELD_UNIQUE.to_owned(), Value::Bool(true)),
    ]);
    let read = IndexDefinition::from_value(&Value::Object(fields)).unwrap();
    assert_eq!(read.fields, vec![Path::field("email")]);
}

#[test]
fn a_stored_route_that_is_not_a_route_is_corruption_rather_than_a_bad_request() {
    // Nothing that reached the catalog could have been an unreadable path:
    // the parser produced it, and the parser cannot spell one. So finding one
    // means the bytes changed underneath, which is a different failure from a
    // caller asking for something impossible.
    let fields = BTreeMap::from([
        (FIELD_ID.to_owned(), number(2)),
        (FIELD_NAMESPACE.to_owned(), number(7)),
        (FIELD_DATABASE.to_owned(), number(3)),
        (FIELD_TABLE.to_owned(), number(11)),
        (FIELD_NAME.to_owned(), Value::from("by_broken")),
        (
            FIELD_FIELDS.to_owned(),
            Value::Array(vec![Value::from("address..city")]),
        ),
        (FIELD_UNIQUE.to_owned(), Value::Bool(false)),
    ]);
    let error = IndexDefinition::from_value(&Value::Object(fields)).unwrap_err();
    assert_eq!(error.code(), "corruption");
}

#[test]
fn a_definition_that_is_not_an_object_is_refused() {
    let error = NamespaceDefinition::from_value(&Value::from("prod")).unwrap_err();
    assert_eq!(error.code(), "corruption");
}

#[test]
fn a_record_count_round_trips_through_the_value_it_is_stored_as() {
    let stored = count(9_001).unwrap();
    assert_eq!(count_of(&stored, "record sequence", "next").unwrap(), 9_001);
}

#[test]
fn a_record_count_the_key_grammar_could_never_spend_is_refused_where_it_is_produced() {
    // A record identity is an `i64`. A count past that could be held here
    // and could never become an identity, so the refusal belongs at the
    // write rather than at the read that would have to explain it.
    let error = count(u64::MAX).unwrap_err();
    assert!(matches!(error, Error::IdSpaceExhausted { .. }), "{error}");
}

#[test]
fn a_stored_record_count_that_is_negative_is_refused_rather_than_wrapped() {
    // Nothing writes one, so one being present means the record is not what
    // this build takes it for — and wrapping would hand the table an
    // identity space it has already spent.
    let error = count_of(
        &Value::Number(Number::Integer(-1)),
        "record sequence",
        "next",
    )
    .unwrap_err();
    assert_eq!(error.code(), "corruption");
}

#[test]
fn an_id_too_large_for_the_identifier_width_is_refused_rather_than_truncated() {
    let fields = BTreeMap::from([
        (
            FIELD_ID.to_owned(),
            Value::Number(Number::Integer(i64::from(u32::MAX) + 1)),
        ),
        (FIELD_NAME.to_owned(), Value::from("x")),
    ]);
    assert!(NamespaceDefinition::from_value(&Value::Object(fields)).is_err());
}
