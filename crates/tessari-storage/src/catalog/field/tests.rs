#![allow(clippy::unwrap_used)]

use super::*;

fn definition(kind: FieldKind) -> FieldDefinition {
    FieldDefinition {
        id: FieldId::new(4),
        namespace: NamespaceId::new(1),
        database: DatabaseId::new(2),
        table: TableId::new(3),
        name: "email".to_owned(),
        kind,
        required: false,
        secret: false,
        default: None,
        assert: None,
        analyzer: None,
    }
}

#[test]
fn a_declaration_round_trips_everything_it_declares() {
    let mut original = definition(FieldKind::Datetime);
    original.required = true;
    original.default = Some("time::now()".to_owned());
    original.analyzer = Some("simple".to_owned());
    assert_eq!(
        FieldDefinition::from_value(&original.to_value()).unwrap(),
        original
    );
}

#[test]
fn a_declaration_written_before_either_existed_requires_nothing_and_fills_nothing() {
    // Every definition on disk today predates both, so nothing migrates.
    let fields = BTreeMap::from([
        (FIELD_ID.to_owned(), number(4)),
        (FIELD_NAMESPACE.to_owned(), number(1)),
        (FIELD_DATABASE.to_owned(), number(2)),
        (FIELD_TABLE.to_owned(), number(3)),
        (FIELD_NAME.to_owned(), Value::from("email")),
        (FIELD_KIND.to_owned(), Value::from("string")),
    ]);
    let read = FieldDefinition::from_value(&Value::Object(fields)).unwrap();
    assert!(!read.required);
    assert_eq!(read.default, None);
}

#[test]
fn a_stored_default_that_is_not_text_is_corruption() {
    let mut fields = BTreeMap::from([
        (FIELD_ID.to_owned(), number(4)),
        (FIELD_NAMESPACE.to_owned(), number(1)),
        (FIELD_DATABASE.to_owned(), number(2)),
        (FIELD_TABLE.to_owned(), number(3)),
        (FIELD_NAME.to_owned(), Value::from("email")),
        (FIELD_KIND.to_owned(), Value::from("string")),
    ]);
    fields.insert(FIELD_DEFAULT.to_owned(), Value::Bool(true));
    let error = FieldDefinition::from_value(&Value::Object(fields)).unwrap_err();
    assert_eq!(error.code(), "corruption");
}

#[test]
fn a_definition_of_every_kind_round_trips() {
    for kind in FieldKind::all() {
        let original = definition(kind.clone());
        assert_eq!(
            FieldDefinition::from_value(&original.to_value()).unwrap(),
            original,
            "{} did not survive the catalog",
            kind.name()
        );
    }
}

#[test]
fn the_kind_is_stored_by_spelling_so_reordering_the_enum_cannot_reinterpret_it() {
    let stored = definition(FieldKind::Decimal).to_value();
    let Value::Object(fields) = &stored else {
        unreachable!("a definition is an object")
    };
    assert_eq!(fields.get(FIELD_KIND), Some(&Value::from("decimal")));
}

#[test]
fn a_kind_this_binary_does_not_know_is_refused_rather_than_guessed() {
    let mut stored = definition(FieldKind::String).to_value();
    if let Value::Object(fields) = &mut stored {
        // The point of the fixture is a kind written by a *newer* binary, so
        // the word has to be one no `FieldKind` holds. `geometry` stood here
        // until the value system gained the type and quietly turned this
        // test into an assertion that a real kind is corruption.
        fields.insert(FIELD_KIND.to_owned(), Value::from("tesseract"));
    }
    let error = FieldDefinition::from_value(&stored).unwrap_err();
    assert_eq!(error.code(), "corruption");
}

#[test]
fn every_kind_this_binary_knows_reads_back_as_itself() {
    // The other half, and the reason the test above could rot unnoticed:
    // nothing asserted that the known kinds *are* known, so adding one broke
    // a negative fixture with no positive one to contradict it.
    for kind in FieldKind::all() {
        let stored = definition(kind.clone()).to_value();
        let read = FieldDefinition::from_value(&stored);
        assert!(
            read.is_ok(),
            "{} did not read back: {:?}",
            kind.name(),
            read.as_ref().err()
        );
        // The assertion above is what fails the test; this only unwraps what
        // it has already established, without a `panic!` the lints refuse.
        let Ok(read) = read else { continue };
        assert_eq!(
            read.kind,
            *kind,
            "{} read back as another kind",
            kind.name()
        );
    }
}

#[test]
fn a_definition_missing_its_kind_is_refused_and_names_the_field() {
    let mut stored = definition(FieldKind::String).to_value();
    if let Value::Object(fields) = &mut stored {
        fields.remove(FIELD_KIND);
    }
    let text = FieldDefinition::from_value(&stored)
        .unwrap_err()
        .to_string();
    assert!(text.contains(FIELD_KIND), "{text}");
}
