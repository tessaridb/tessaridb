#![allow(clippy::unwrap_used)]

use super::*;

#[test]
fn a_declaration_round_trips_with_and_without_a_limit() {
    for declared in [
        SpaceDeclaration::default(),
        SpaceDeclaration {
            limit: Some(SpaceLimit {
                max: 10_000,
                eviction: Eviction::Modified,
            }),
        },
        SpaceDeclaration {
            limit: Some(SpaceLimit {
                max: 3,
                eviction: Eviction::Refuse,
            }),
        },
    ] {
        assert_eq!(
            SpaceDeclaration::from_value(&declared.to_value()).unwrap(),
            declared
        );
    }
}

#[test]
fn a_limit_of_zero_or_an_unknown_rule_is_malformed() {
    let zero = Value::Object(BTreeMap::from([
        (FIELD_MAX.to_owned(), Value::Number(Number::Integer(0))),
        (FIELD_EVICT.to_owned(), Value::from(EVICT_NONE)),
    ]));
    assert!(SpaceDeclaration::from_value(&zero).is_err());
    let unknown = Value::Object(BTreeMap::from([
        (FIELD_MAX.to_owned(), Value::Number(Number::Integer(5))),
        (FIELD_EVICT.to_owned(), Value::from("lru")),
    ]));
    assert!(SpaceDeclaration::from_value(&unknown).is_err());
}
