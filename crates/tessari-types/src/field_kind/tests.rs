use std::collections::{BTreeMap, BTreeSet};
use std::ops::Bound;

use super::*;
use crate::{Datetime, Duration, RecordId, RecordRef, TableId, ValueRange};

/// One value of each of the seventeen types, in the order `Value` declares
/// them. Anything that must hold for the whole value system is asserted
/// against this list rather than against a sample.
fn one_of_each() -> Vec<Value> {
    vec![
        Value::None,
        Value::Null,
        Value::Bool(true),
        Value::Number(Number::Integer(7)),
        Value::from("text"),
        Value::Bytes(vec![1, 2]),
        Value::Duration(Duration::from_seconds(1)),
        Value::Datetime(Datetime::from_seconds(0)),
        Value::Uuid([0; 16]),
        Value::Table(TableId::new(1)),
        Value::Record(RecordRef::new(TableId::new(1), RecordId::Int(1))),
        Value::Array(vec![Value::Bool(false)]),
        Value::Object(BTreeMap::new()),
        Value::Range(Box::new(ValueRange::new(
            Bound::Included(Value::Number(Number::Integer(1))),
            Bound::Excluded(Value::Number(Number::Integer(2))),
        ))),
        Value::Set(BTreeSet::new()),
    ]
}

#[test]
fn every_kind_has_its_own_spelling_and_reads_back() {
    for kind in FieldKind::all() {
        assert_eq!(FieldKind::parse(&kind.name()), Some(kind.clone()));
    }
    let names: BTreeSet<Cow<'static, str>> = FieldKind::all().iter().map(FieldKind::name).collect();
    assert_eq!(names.len(), FieldKind::all().len(), "a spelling is reused");
}

#[test]
fn a_spelling_is_read_whatever_its_case() {
    assert_eq!(FieldKind::parse("DATETIME"), Some(FieldKind::Datetime));
    assert_eq!(FieldKind::parse("Decimal"), Some(FieldKind::Decimal));
    assert_eq!(FieldKind::parse("integer"), None);
}

#[test]
fn every_value_type_is_nameable_by_some_kind() {
    for value in one_of_each() {
        if matches!(value, Value::None | Value::Null) {
            continue;
        }
        assert!(
            FieldKind::all()
                .iter()
                .any(|kind| *kind != FieldKind::Any && kind.accepts(&value)),
            "no kind accepts {}",
            value.type_name()
        );
    }
}

#[test]
fn a_kind_accepts_its_own_type_and_refuses_the_others() {
    for value in one_of_each() {
        if matches!(value, Value::None | Value::Null) {
            continue;
        }
        let accepting: Vec<Cow<'static, str>> = FieldKind::all()
            .iter()
            .filter(|kind| kind.accepts(&value))
            .map(FieldKind::name)
            .collect();
        // `any` accepts everything and `number` accepts all three numeric
        // forms, so a number is accepted by three kinds and everything else
        // by exactly two.
        let expected = if matches!(value, Value::Number(_)) {
            3
        } else {
            2
        };
        assert_eq!(
            accepting.len(),
            expected,
            "{value:?} accepted by {accepting:?}"
        );
    }
}

#[test]
fn absent_and_null_satisfy_every_kind() {
    for kind in FieldKind::all() {
        assert!(kind.accepts(&Value::None), "{} refused none", kind.name());
        assert!(kind.accepts(&Value::Null), "{} refused null", kind.name());
    }
}

#[test]
fn the_three_numeric_forms_are_kept_apart() {
    let integer = Value::Number(Number::Integer(1));
    let float = Value::Number(Number::float(1.0));
    assert!(FieldKind::Int.accepts(&integer));
    assert!(!FieldKind::Int.accepts(&float));
    assert!(FieldKind::Float.accepts(&float));
    assert!(!FieldKind::Float.accepts(&integer));
    assert!(FieldKind::Number.accepts(&integer));
    assert!(FieldKind::Number.accepts(&float));
}

/// The union spells itself as its members, and reads back as the same set.
///
/// The catalog stores a kind by its spelling, so this round trip *is* the
/// storage format. A union that spelled itself one way and read back another
/// would be a declaration that changes meaning when the store reopens.
#[test]
fn a_union_round_trips_through_its_spelling() {
    let kind = FieldKind::union(vec!["published".to_owned(), "draft".to_owned()])
        .expect("a union of at least one member");
    assert_eq!(kind.name(), "'draft' | 'published'");
    assert_eq!(FieldKind::parse(&kind.name()), Some(kind));
}

/// Two declarations naming one set are one type, however they were typed.
#[test]
fn a_union_is_a_set_and_not_a_list() {
    let written_one_way = FieldKind::union(vec!["b".to_owned(), "a".to_owned(), "b".to_owned()])
        .expect("a union of at least one member");
    let written_another = FieldKind::union(vec!["a".to_owned(), "b".to_owned()])
        .expect("a union of at least one member");
    assert_eq!(written_one_way, written_another);
}

/// A member carrying the characters the spelling uses survives the trip.
///
/// The separator needs no escape because a member is always quoted; the
/// quote and the backslash do, and getting that wrong would silently widen
/// or narrow a declared type when the catalog is read back.
#[test]
fn a_member_holding_a_quote_or_a_backslash_survives() {
    for awkward in ["it's", r"back\slash", "a | b", "'", r"\"] {
        let kind =
            FieldKind::union(vec![awkward.to_owned()]).expect("a union of at least one member");
        assert_eq!(
            FieldKind::parse(&kind.name()),
            Some(kind.clone()),
            "{awkward:?} did not survive {:?}",
            kind.name()
        );
    }
}

/// A union accepts its members, and refuses everything else including
/// other text.
#[test]
fn a_union_refuses_a_string_it_does_not_name() {
    let kind = FieldKind::union(vec!["draft".to_owned(), "published".to_owned()])
        .expect("a union of at least one member");
    assert!(kind.accepts(&Value::from("draft")));
    assert!(kind.accepts(&Value::from("published")));
    assert!(!kind.accepts(&Value::from("archived")));
    assert!(!kind.accepts(&Value::Number(Number::Integer(1))));
    // The two values every kind accepts, which a subset does not change.
    assert!(kind.accepts(&Value::None));
    assert!(kind.accepts(&Value::Null));
}

/// A type nothing could satisfy is a mistake, not a constraint.
#[test]
fn an_empty_union_is_refused_at_construction() {
    assert_eq!(FieldKind::union(Vec::new()), None);
}

/// A malformed spelling is refused rather than read as far as it goes.
///
/// A kind read out of the catalog decides what a stored value may be, so a
/// partial reading would relax a constraint with nothing saying so.
#[test]
fn a_malformed_union_is_refused_rather_than_half_read() {
    for broken in [
        "'draft",
        "'draft' |",
        "'draft' 'published'",
        "'a' | b",
        "'a' ,",
    ] {
        assert_eq!(FieldKind::parse(broken), None, "{broken:?} was accepted");
    }
}

/// The simple kinds are unaffected: their spelling is the same word it was,
/// so a store written before unions existed still reads.
#[test]
fn a_simple_kind_still_spells_itself_as_its_bare_word() {
    assert_eq!(FieldKind::String.name(), "string");
    assert_eq!(FieldKind::parse("string"), Some(FieldKind::String));
}

/// The round trip is the whole reason a width may be stored at all: the
/// catalog keeps the spelling, so a width that writes one way and reads
/// another is a declaration that changes meaning when the store reopens.
#[test]
fn a_width_survives_the_catalog_spelling_it_is_stored_as() {
    for width in [1_usize, 2, 768, 1536, 4096] {
        let kind = FieldKind::vector(width).expect("a width of at least one");
        let spelled = kind.name().into_owned();
        assert_eq!(spelled, format!("vector<{width}>"));
        assert_eq!(
            FieldKind::parse(&spelled),
            Some(kind),
            "{spelled} did not read back"
        );
    }
}

/// Case-insensitive like every other kind, so one type does not answer the
/// spelling rule differently from the other sixteen.
#[test]
fn a_width_reads_in_any_case() {
    let expected = FieldKind::vector(8);
    for spelling in ["vector<8>", "VECTOR<8>", "Vector<8>", "  vector<8>  "] {
        assert_eq!(
            FieldKind::parse(spelling),
            expected,
            "{spelling:?} was refused"
        );
    }
}

/// Zero and its neighbours: a width below one is refused at construction,
/// and a width-less `vector` is not a type — an array whose length nobody
/// declared is the `array` this language already has.
#[test]
fn a_width_below_one_is_refused_and_so_is_no_width_at_all() {
    assert_eq!(FieldKind::vector(0), None);
    for broken in [
        "vector<0>",
        "vector",
        "vector<>",
        "vector<-1>",
        "vector<x>",
        "vector<8",
    ] {
        assert_eq!(FieldKind::parse(broken), None, "{broken:?} was accepted");
    }
}

/// The refusal this kind exists for. Width **and** contents, because either
/// alone lets through the value the declaration was written to stop.
#[test]
fn a_declared_width_accepts_that_width_and_nothing_else() {
    let kind = FieldKind::vector(3).expect("a width of at least one");
    let number = |held: i64| Value::Number(Number::Integer(held));

    assert!(kind.accepts(&Value::Array(vec![number(1), number(2), number(3)])));
    // Mixed numeric forms are still numbers; the distance functions read
    // all three, so the declaration may not be narrower than they are.
    assert!(kind.accepts(&Value::Array(vec![
        number(1),
        Value::Number(Number::Float(2.5)),
        number(3),
    ])));

    // Too short, too long, not numbers, not an array at all.
    assert!(!kind.accepts(&Value::Array(vec![number(1), number(2)])));
    assert!(!kind.accepts(&Value::Array(vec![
        number(1),
        number(2),
        number(3),
        number(4)
    ])));
    assert!(!kind.accepts(&Value::Array(vec![number(1), Value::from("2"), number(3)])));
    assert!(!kind.accepts(&Value::from("[1, 2, 3]")));

    // And the two values every kind accepts, which a width does not change:
    // a declared type does not make a field mandatory.
    assert!(kind.accepts(&Value::None));
    assert!(kind.accepts(&Value::Null));
}
