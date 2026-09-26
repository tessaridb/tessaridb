#![allow(clippy::unwrap_used)]

use super::*;

#[test]
fn every_shape_round_trips() {
    let shapes = [
        TopicDeclaration::default(),
        TopicDeclaration {
            retain: Some(Duration::from_seconds(604_800)),
            max_bytes: Some(4096),
            public: Some(PublicAppend {
                rate: 100,
                per: Duration::from_seconds(60),
            }),
        },
        TopicDeclaration {
            retain: None,
            max_bytes: Some(1),
            public: None,
        },
    ];
    for shape in shapes {
        assert_eq!(
            TopicDeclaration::from_value(&shape.to_value()).unwrap(),
            shape
        );
    }
}

#[test]
fn a_rate_without_its_window_is_malformed() {
    let value = Value::Object(BTreeMap::from([(FIELD_PUBLIC_RATE.to_owned(), integer(5))]));
    assert!(matches!(
        TopicDeclaration::from_value(&value),
        Err(Error::CatalogMalformed { .. })
    ));
}

#[test]
fn a_size_of_zero_is_malformed() {
    let value = Value::Object(BTreeMap::from([(
        FIELD_MAX_BYTES.to_owned(),
        Value::Number(Number::Integer(0)),
    )]));
    assert!(matches!(
        TopicDeclaration::from_value(&value),
        Err(Error::CatalogMalformed { .. })
    ));
}
