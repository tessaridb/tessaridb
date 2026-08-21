//! Addressing a value that sits inside another one.
//!
//! A record payload is already an arbitrarily nested tree — [`Value::Object`]
//! and [`Value::Array`] are two of the fifteen types, and neither has ever had a
//! depth limit. What was missing is a way to *name* something inside it, so that
//! a filter, an index and a projection can all say `address.city` and mean the
//! same value.
//!
//! # One walk, not three
//!
//! The same route is followed in three layers: the session filters with it, the
//! storage layer projects index entries with it, and the executor will read it
//! for projections and ordering. Three implementations would compile. They would
//! also drift, and the way they drift is the expensive one — a filter deciding a
//! record does not match while the index says it does, which is an answer
//! changing with nothing raised. So the walk lives here, in the leaf crate all
//! three already depend on, and is written once.
//!
//! # A path that does not resolve is not an error
//!
//! A missing intermediate, an object addressed by position, an array addressed
//! by name, a position past the end: every one of them yields nothing, and
//! nothing means the filter does not match and the index does not index. That is
//! not leniency. It is the rule the store already applies one level up — a
//! record missing an indexed field is not indexed at all — carried downward
//! unchanged. Raising instead would make a document store refuse documents whose
//! shapes differ, which is the reason to have one.
//!
//! # The contract on names
//!
//! A name segment holds no `.`, `[` or `]`, because those three characters are
//! what separates one segment from the next. [`Path::parse`] enforces it and is
//! the only constructor that accepts arbitrary text; the constructors that take
//! pieces trust their caller, and their callers pass either a literal or a token
//! the lexer produced, which cannot contain a delimiter.
//!
//! The consequence is stated rather than discovered: a field whose real name
//! contains one of those characters cannot be addressed by a path. Every path
//! language makes that trade, and the alternative — a quoting rule — is grammar
//! nobody has asked for yet.

use core::fmt;
use core::iter::Peekable;
use core::str::Chars;

use crate::value::Value;

/// One step of a route into a nested value.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Step {
    /// `.name` — the value held under a field of an object.
    Field(String),
    /// `[n]` — the value at a position of an array.
    ///
    /// Held as [`u64`] rather than [`usize`] because a path is **stored** in the
    /// catalog, and a stored form whose range depends on the machine that wrote
    /// it is a format with a footnote. The narrowing to a slice position happens
    /// at the moment of the lookup, where it is checked.
    Index(u64),
}

/// A route from a record to a value inside it.
///
/// The root is always a field name, because a record is an object and there is
/// nothing above it to address into.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Path {
    root: String,
    steps: Vec<Step>,
}

impl Path {
    /// A route from a field name and the steps below it.
    ///
    /// The caller warrants that every name segment is free of `.`, `[` and `]`;
    /// see the module documentation for why that is a contract rather than a
    /// check. Text from outside goes through [`Path::parse`].
    #[must_use]
    pub fn new(root: String, steps: Vec<Step>) -> Self {
        Self { root, steps }
    }

    /// A route naming one top-level field, and nothing below it.
    #[must_use]
    pub fn field(name: &str) -> Self {
        Self {
            root: name.to_owned(),
            steps: Vec::new(),
        }
    }

    /// The field the route starts at.
    #[must_use]
    pub fn root(&self) -> &str {
        &self.root
    }

    /// The steps below the root, in the order they are followed.
    #[must_use]
    pub fn steps(&self) -> &[Step] {
        &self.steps
    }

    /// Read a route back from the text it was written as.
    ///
    /// Returns `None` when the text is not a path: an empty segment, a position
    /// that is not a run of digits or does not close, or a stray delimiter. The
    /// caller decides what that means — for a value read out of the catalog it
    /// is corruption, and for one typed by a user it is a parse error.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let mut characters = text.chars().peekable();
        let root = name(&mut characters)?;
        let mut steps = Vec::new();
        while let Some(character) = characters.next() {
            match character {
                '.' => steps.push(Step::Field(name(&mut characters)?)),
                '[' => steps.push(Step::Index(position(&mut characters)?)),
                _ => return None,
            }
        }
        Some(Self { root, steps })
    }

    /// The value this route reaches, or `None` when it reaches nothing.
    ///
    /// `Value::None` is returned as itself rather than as nothing: it is a value
    /// that says the field is absent, and flattening it here would take away the
    /// caller's ability to tell "no such route" from "a route to an absence".
    #[must_use]
    pub fn resolve<'value>(&self, value: &'value Value) -> Option<&'value Value> {
        let Value::Object(fields) = value else {
            return None;
        };
        let mut current = fields.get(&self.root)?;
        for step in &self.steps {
            current = match (step, current) {
                (Step::Field(name), Value::Object(fields)) => fields.get(name)?,
                (Step::Index(at), Value::Array(items)) => items.get(usize::try_from(*at).ok()?)?,
                _ => return None,
            };
        }
        Some(current)
    }

    /// The value this route reaches, so a caller can change it in place.
    ///
    /// The mirror of [`Path::resolve`], and it exists because there was until
    /// now nothing in this store that *wrote* through a route — every walk read.
    /// Following a reference has to put the record it found where the reference
    /// was, and rebuilding the object around it by hand at each step is the same
    /// walk written a second time, differently.
    ///
    /// `None` for exactly the routes `resolve` answers `None` for, so the two
    /// cannot disagree about which routes exist.
    #[must_use]
    pub fn resolve_mut<'value>(&self, value: &'value mut Value) -> Option<&'value mut Value> {
        let Value::Object(fields) = value else {
            return None;
        };
        let mut current = fields.get_mut(&self.root)?;
        for step in &self.steps {
            current = match (step, current) {
                (Step::Field(name), Value::Object(fields)) => fields.get_mut(name)?,
                (Step::Index(at), Value::Array(items)) => {
                    items.get_mut(usize::try_from(*at).ok()?)?
                }
                _ => return None,
            };
        }
        Some(current)
    }
}

impl fmt::Display for Path {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.root)?;
        for step in &self.steps {
            match step {
                Step::Field(name) => write!(f, ".{name}")?,
                Step::Index(at) => write!(f, "[{at}]")?,
            }
        }
        Ok(())
    }
}

/// A name segment, read up to but not including the delimiter that ends it.
fn name(characters: &mut Peekable<Chars<'_>>) -> Option<String> {
    let mut read = String::new();
    while let Some(character) = characters.peek() {
        if matches!(character, '.' | '[' | ']') {
            break;
        }
        read.push(*character);
        characters.next();
    }
    (!read.is_empty()).then_some(read)
}

/// A position, read up to and including its closing bracket.
fn position(characters: &mut Peekable<Chars<'_>>) -> Option<u64> {
    let mut digits = String::new();
    for character in characters.by_ref() {
        if character == ']' {
            return digits.parse().ok();
        }
        if !character.is_ascii_digit() {
            return None;
        }
        digits.push(character);
    }
    None
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic)]

    use std::collections::BTreeMap;

    use super::{Path, Step};
    use crate::number::Number;
    use crate::value::Value;

    fn object(pairs: [(&str, Value); 1]) -> Value {
        Value::Object(BTreeMap::from(pairs.map(|(k, v)| (k.to_owned(), v))))
    }

    fn text(value: &str) -> Value {
        Value::String(value.to_owned())
    }

    #[test]
    fn a_path_round_trips_through_its_own_spelling() {
        // The catalog stores a path as the text it was written as, so a spelling
        // that read back as a different route would index one field and filter
        // another.
        for spelling in [
            "email",
            "address.city",
            "tags[0]",
            "history[2].by",
            "a.b.c.d",
            "a[0][1]",
        ] {
            let path = Path::parse(spelling).expect("a path");
            assert_eq!(path.to_string(), spelling);
            assert_eq!(Path::parse(&path.to_string()), Some(path));
        }
    }

    #[test]
    fn what_is_not_a_path_is_refused_rather_than_half_read() {
        for spelling in [
            "", ".", "a.", ".a", "a..b", "a[", "a[]", "a[x]", "a[0", "a]", "a[0]]",
        ] {
            assert_eq!(Path::parse(spelling), None, "{spelling}");
        }
    }

    #[test]
    fn a_route_reaches_the_value_under_it() {
        let record = object([("address", object([("city", text("Paris"))]))]);
        let path = Path::parse("address.city").expect("a path");
        assert_eq!(path.resolve(&record), Some(&text("Paris")));
    }

    #[test]
    fn a_position_reaches_into_an_array() {
        let record = object([(
            "tags",
            Value::Array(vec![text("urgent"), object([("by", text("ada"))])]),
        )]);
        assert_eq!(
            Path::parse("tags[0]").expect("a path").resolve(&record),
            Some(&text("urgent"))
        );
        assert_eq!(
            Path::parse("tags[1].by").expect("a path").resolve(&record),
            Some(&text("ada"))
        );
    }

    #[test]
    fn every_way_of_reaching_nothing_reaches_nothing() {
        let record = object([(
            "address",
            object([("city", Value::Array(vec![text("Paris")]))]),
        )]);
        for spelling in [
            // No such root.
            "postcode",
            // No such field below one that exists.
            "address.street",
            // An object addressed by position.
            "address[0]",
            // An array addressed by name.
            "address.city.first",
            // Past the end.
            "address.city[1]",
            // Through a leaf.
            "address.city[0].x",
        ] {
            let path = Path::parse(spelling).expect("a path");
            assert_eq!(path.resolve(&record), None, "{spelling}");
        }
    }

    #[test]
    fn a_record_that_is_not_an_object_has_no_routes_into_it() {
        // A space holds single values. Asking one for a field is a question with
        // no answer rather than a failure.
        let path = Path::field("anything");
        assert_eq!(path.resolve(&Value::Number(Number::Integer(7))), None);
        assert_eq!(path.resolve(&Value::Null), None);
    }

    #[test]
    fn a_route_to_an_absence_is_not_the_same_as_no_route() {
        // `Value::None` is a value saying the field is not there. Collapsing it
        // into "no such route" here would take the distinction away from every
        // caller, and the store's whole value system rests on keeping it.
        let record = object([("email", Value::None)]);
        assert_eq!(Path::field("email").resolve(&record), Some(&Value::None));
        assert_eq!(Path::field("other").resolve(&record), None);
    }

    #[test]
    fn a_named_field_is_a_path_of_one_step() {
        let path = Path::field("email");
        assert_eq!(path.root(), "email");
        assert_eq!(path.steps(), &[]);
        assert_eq!(path.to_string(), "email");

        let nested = Path::new("a".to_owned(), vec![Step::Field("b".to_owned())]);
        assert_eq!(nested.steps().len(), 1);
        assert_eq!(nested.to_string(), "a.b");
    }
}
