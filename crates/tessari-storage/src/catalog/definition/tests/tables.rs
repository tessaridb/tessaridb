use super::*;

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
