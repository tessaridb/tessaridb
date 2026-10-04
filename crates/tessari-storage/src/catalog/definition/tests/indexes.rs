use super::*;

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
