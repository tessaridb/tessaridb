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

use std::borrow::Cow;

use crate::{Number, Value};

/// The type a field is declared to hold.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
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
    /// A shape on the sphere.
    Geometry,
    /// A pattern, held rather than executed.
    Regex,
    /// One of a fixed set of strings, and nothing else.
    ///
    /// The one kind that is not a type of the value system but a **subset** of
    /// one, which is why it carries its own values rather than naming a variant.
    /// `TYPE 'draft' | 'published'` is the declaration a status column has
    /// always wanted: `string` is true and says nothing, and an `ASSERT` says it
    /// but says it in a place a reader of the schema does not look.
    ///
    /// Held sorted and deduplicated. A declared type is a set, and a set that
    /// remembers the order somebody typed it in is two values for one fact —
    /// the same rule a grant's verbs and fields already follow.
    Literal(Vec<String>),
}

/// What separates the members of a literal union, in the catalog and on screen.
const UNION: &str = " | ";

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
    FieldKind::Geometry,
    FieldKind::Regex,
];

impl FieldKind {
    /// How the kind is written in the language and stored in the catalog.
    ///
    /// One spelling serves both, so a definition read back out of the catalog
    /// says what its author typed.
    #[must_use]
    pub fn name(&self) -> Cow<'static, str> {
        if let Self::Literal(members) = self {
            return Cow::Owned(
                members
                    .iter()
                    .map(|member| quote(member))
                    .collect::<Vec<_>>()
                    .join(UNION),
            );
        }
        Cow::Borrowed(self.simple_name())
    }

    /// The spelling of a kind that has no parameters.
    ///
    /// Split out so it stays `const` and allocation-free: every kind but one is
    /// a fixed word, and the catalog reads and writes those on every field of
    /// every table.
    #[must_use]
    const fn simple_name(&self) -> &'static str {
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
            Self::Geometry => "geometry",
            Self::Regex => "regex",
            // Unreachable: the caller returns before this. Named rather than
            // wildcarded so a second parameterised kind fails to compile here
            // instead of silently spelling itself as its own placeholder.
            Self::Literal(_) => "",
        }
    }

    /// Read a kind back from its spelling.
    ///
    /// Case-insensitive, because a keyword elsewhere in the language is.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        let trimmed = name.trim();
        if trimmed.starts_with('\'') {
            return parse_union(trimmed);
        }
        ALL.iter()
            .find(|kind| kind.simple_name().eq_ignore_ascii_case(trimmed))
            .cloned()
    }

    /// A union of the given members, sorted and deduplicated.
    ///
    /// The constructor rather than the variant, so that no caller can build one
    /// carrying the same member twice or in an order that would make two equal
    /// declarations compare unequal.
    ///
    /// Returns `None` for an empty set, which no value could satisfy: a type
    /// nothing can hold is a declaration that refuses every write, and that is a
    /// mistake rather than a constraint.
    #[must_use]
    pub fn union(members: Vec<String>) -> Option<Self> {
        let mut members = members;
        members.sort_unstable();
        members.dedup();
        if members.is_empty() {
            return None;
        }
        Some(Self::Literal(members))
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
    pub fn accepts(&self, value: &Value) -> bool {
        if matches!(value, Value::None | Value::Null) || matches!(self, Self::Any) {
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
            Self::Geometry => matches!(value, Value::Geometry(_)),
            Self::Regex => matches!(value, Value::Regex(_)),
            // A subset of `string`, so it refuses on two counts: a value that is
            // not text at all, and text that is not one of the named members.
            Self::Literal(members) => {
                matches!(value, Value::String(held) if members.iter().any(|member| member == held))
            }
        }
    }
}

/// One member of a union, as the catalog spells it.
///
/// Escaped so that a member containing a quote or a backslash survives the round
/// trip. The separator needs no escaping because a member is always quoted, so a
/// bare `|` inside one is unambiguous.
fn quote(member: &str) -> String {
    let mut spelled = String::with_capacity(member.len().saturating_add(2));
    spelled.push('\'');
    for character in member.chars() {
        if character == '\\' || character == '\'' {
            spelled.push('\\');
        }
        spelled.push(character);
    }
    spelled.push('\'');
    spelled
}

/// Read a union back from the spelling [`quote`] produced.
///
/// Returns `None` on anything it does not fully understand rather than on a
/// best effort: a kind read out of the catalog decides what a stored value is
/// allowed to be, so guessing at a malformed one would relax a constraint
/// silently. The caller treats `None` as catalog corruption, which is what it is.
fn parse_union(spelling: &str) -> Option<FieldKind> {
    let mut members = Vec::new();
    let mut characters = spelling.chars().peekable();
    loop {
        while characters.peek().is_some_and(|held| held.is_whitespace()) {
            characters.next();
        }
        if characters.next() != Some('\'') {
            return None;
        }
        let mut member = String::new();
        loop {
            match characters.next()? {
                '\'' => break,
                '\\' => member.push(characters.next()?),
                held => member.push(held),
            }
        }
        members.push(member);
        while characters.peek().is_some_and(|held| held.is_whitespace()) {
            characters.next();
        }
        match characters.next() {
            None => break,
            Some('|') => {}
            Some(_) => return None,
        }
    }
    FieldKind::union(members)
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
            assert_eq!(FieldKind::parse(&kind.name()), Some(kind.clone()));
        }
        let names: BTreeSet<Cow<'static, str>> =
            FieldKind::all().iter().map(FieldKind::name).collect();
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
        let written_one_way =
            FieldKind::union(vec!["b".to_owned(), "a".to_owned(), "b".to_owned()])
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
}
