//! What a field is allowed to hold.
//!
//! A [`FieldKind`] is the type-level counterpart of [`Value`]: the declaration a
//! table makes about a field, against which each written value is checked. It
//! lives beside the value system rather than in the catalog because the two must
//! not drift — a kind that no value can satisfy, or a value no kind can name,
//! would be a hole nothing detects.
//!
//! # The set is the value system's, plus two
//!
//! Every one of the fifteen value types is nameable. Two more kinds exist
//! because the value system's shape does not match one-to-one what a declaration
//! wants to say:
//!
//! - [`FieldKind::Any`] accepts everything. It is the schemaless default made
//!   explicit, so that a field can be declared — and so appear in a `SCHEMAFULL`
//!   table — without its type being narrowed.
//! - [`FieldKind::Number`] accepts all three numeric forms, while
//!   [`Int`](FieldKind::Int), [`Float`](FieldKind::Float) and
//!   [`Decimal`](FieldKind::Decimal) each accept one. [`Value::type_name`]
//!   reports all three as `number`, so a declaration that could only say
//!   `number` would be unable to keep money exact.

use crate::{Number, Value};

/// The type a field is declared to hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FieldKind {
    /// Any value at all.
    Any,
    /// A boolean.
    Bool,
    /// Any of the three numeric forms.
    Number,
    /// A signed integer.
    Int,
    /// A binary floating-point number.
    Float,
    /// An exact decimal.
    Decimal,
    /// Text.
    String,
    /// Opaque bytes.
    Bytes,
    /// A span of time.
    Duration,
    /// A point in time.
    Datetime,
    /// A universally unique identifier.
    Uuid,
    /// A reference to a table.
    Table,
    /// A reference to one record.
    Record,
    /// An ordered sequence.
    Array,
    /// A map from field name to value.
    Object,
    /// A span between two values.
    Range,
    /// A collection with no duplicates.
    Set,
}

/// Every kind, in declaration order.
///
/// Used by name lookup and by the tests that assert the set stays complete.
const ALL: &[FieldKind] = &[
    FieldKind::Any,
    FieldKind::Bool,
    FieldKind::Number,
    FieldKind::Int,
    FieldKind::Float,
    FieldKind::Decimal,
    FieldKind::String,
    FieldKind::Bytes,
    FieldKind::Duration,
    FieldKind::Datetime,
    FieldKind::Uuid,
    FieldKind::Table,
    FieldKind::Record,
    FieldKind::Array,
    FieldKind::Object,
    FieldKind::Range,
    FieldKind::Set,
];

impl FieldKind {
    /// How the kind is written in the language and stored in the catalog.
    ///
    /// One spelling serves both, so a definition read back out of the catalog
    /// says what its author typed.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Any => "any",
            Self::Bool => "bool",
            Self::Number => "number",
            Self::Int => "int",
            Self::Float => "float",
            Self::Decimal => "decimal",
            Self::String => "string",
            Self::Bytes => "bytes",
            Self::Duration => "duration",
            Self::Datetime => "datetime",
            Self::Uuid => "uuid",
            Self::Table => "table",
            Self::Record => "record",
            Self::Array => "array",
            Self::Object => "object",
            Self::Range => "range",
            Self::Set => "set",
        }
    }

    /// Read a kind back from its spelling.
    ///
    /// Case-insensitive, because a keyword elsewhere in the language is.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        ALL.iter()
            .copied()
            .find(|kind| kind.name().eq_ignore_ascii_case(name))
    }

    /// Every kind that can be declared.
    #[must_use]
    pub const fn all() -> &'static [Self] {
        ALL
    }

    /// Whether a field holding `value` satisfies this declaration.
    ///
    /// **Two values satisfy every kind**, and the exceptions are here rather
    /// than at the call site so that one rule cannot be applied in one place and
    /// forgotten in another:
    ///
    /// - [`Value::None`] — the field is not there, so there is nothing to check.
    ///   This is the rule an index already applies: a record missing an indexed
    ///   field is not indexed, rather than indexed as absent. A declared type
    ///   therefore does **not** make a field mandatory.
    /// - [`Value::Null`] — the field is there and holds nothing. Following SQL,
    ///   a typed column accepts null; requiring a value is a separate constraint
    ///   that this milestone does not have.
    #[must_use]
    pub fn accepts(self, value: &Value) -> bool {
        if matches!(value, Value::None | Value::Null) || self == Self::Any {
            return true;
        }
        match self {
            Self::Any => true,
            Self::Bool => matches!(value, Value::Bool(_)),
            Self::Number => matches!(value, Value::Number(_)),
            Self::Int => matches!(value, Value::Number(Number::Integer(_))),
            Self::Float => matches!(value, Value::Number(Number::Float(_))),
            Self::Decimal => matches!(value, Value::Number(Number::Decimal(_))),
            Self::String => matches!(value, Value::String(_)),
            Self::Bytes => matches!(value, Value::Bytes(_)),
            Self::Duration => matches!(value, Value::Duration(_)),
            Self::Datetime => matches!(value, Value::Datetime(_)),
            Self::Uuid => matches!(value, Value::Uuid(_)),
            Self::Table => matches!(value, Value::Table(_)),
            Self::Record => matches!(value, Value::Record(_)),
            Self::Array => matches!(value, Value::Array(_)),
            Self::Object => matches!(value, Value::Object(_)),
            Self::Range => matches!(value, Value::Range(_)),
            Self::Set => matches!(value, Value::Set(_)),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::ops::Bound;

    use super::*;
    use crate::{Datetime, Duration, RecordId, RecordRef, TableId, ValueRange};

    /// One value of each of the fifteen types, in the order `Value` declares
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
            assert_eq!(FieldKind::parse(kind.name()), Some(*kind));
        }
        let names: BTreeSet<&str> = FieldKind::all().iter().map(|kind| kind.name()).collect();
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
            let accepting: Vec<&'static str> = FieldKind::all()
                .iter()
                .filter(|kind| kind.accepts(&value))
                .map(|kind| kind.name())
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
}
