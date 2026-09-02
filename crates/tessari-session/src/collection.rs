//! Opening an object, and reshaping an array.
//!
//! # What earns a place here
//!
//! The language's rule is that a function exists when nothing already says what
//! it says. Each of the nine below passes it for the same reason: a **path takes
//! a literal name or position and no range**, so there is no way to reach an
//! object's field names, to reorder an array, or to take a run out of the middle
//! of one. `array::contains` and `string::starts_with` were rejected in the same
//! pass, because `CONTAINS` and `LIKE 'x%'` already say those.
//!
//! # Every ordering question here has one answer, and the other is sayable
//!
//! `array::sort` sorts **ascending**, and there is no `array::sort_desc`,
//! because `array::reverse(array::sort(x))` is it. `array::distinct` keeps the
//! **first** occurrence and so preserves the order it was given, because
//! `array::sort(array::distinct(x))` recovers the sorted form while the reverse
//! recovery is impossible — order thrown away cannot be got back. Each time the
//! rule is the same: of two candidate behaviours, take the one the other can be
//! written from.

use tessari_ql::{Function, Span};
use tessari_types::Value;

use crate::error::{Error, Result};

/// The field names of an object, in the object's own order.
///
/// That order is name order, because an object is stored as a map keyed by name
/// so that two objects with the same content encode to the same bytes. It is
/// therefore the same order [`values`] walks, which is what makes the two
/// correspond position by position — a property the caller has no other way to
/// establish, since nothing lets them zip two arrays.
pub(crate) fn keys(object: &std::collections::BTreeMap<String, Value>) -> Value {
    Value::Array(
        object
            .keys()
            .map(|name| Value::from(name.as_str()))
            .collect(),
    )
}

/// The values of an object, in the same order [`keys`] gives the names.
pub(crate) fn values(object: &std::collections::BTreeMap<String, Value>) -> Value {
    Value::Array(object.values().cloned().collect())
}

/// Each value once, keeping the first occurrence.
///
/// Equality is the value system's, which unifies the numeric kinds — so `1`,
/// `1.0` and `dec 1` are one value here, exactly as they are one member of a
/// set. A `distinct` that disagreed with `=` would be a second notion of
/// sameness in a language that has one.
pub(crate) fn distinct(items: &[Value]) -> Value {
    let mut held: Vec<Value> = Vec::with_capacity(items.len());
    for item in items {
        if !held.contains(item) {
            held.push(item.clone());
        }
    }
    Value::Array(held)
}

/// The values in ascending order.
///
/// The value system's **declared total order**, which spans types — so an array
/// holding a number and a string sorts rather than failing, and sorts the way
/// the same values would sort in an index. A comparison here that disagreed with
/// the stored order would be an answer that changes when an index appears.
pub(crate) fn sort(items: &[Value]) -> Value {
    let mut held = items.to_vec();
    held.sort();
    Value::Array(held)
}

/// The values back to front.
pub(crate) fn reverse(items: &[Value]) -> Value {
    let mut held = items.to_vec();
    held.reverse();
    Value::Array(held)
}

/// One level of nesting removed.
///
/// **One level, not all of them.** A deep flatten cannot be undone and cannot be
/// bounded: an array holding an array of arrays has no natural depth, and a
/// caller who wanted two levels can write `array::flatten` twice, while a caller
/// who got a deep flatten and wanted one level has no way back. An element that
/// is not an array is kept as it is rather than refused, since a mixed array is
/// an ordinary shape in a store that holds documents of differing shapes.
pub(crate) fn flatten(items: &[Value]) -> Value {
    let mut held = Vec::with_capacity(items.len());
    for item in items {
        match item {
            Value::Array(inner) => held.extend(inner.iter().cloned()),
            other => held.push(other.clone()),
        }
    }
    Value::Array(held)
}

/// The elements as one text, separated.
///
/// Each element is read by **`type::string`'s rule** rather than by a rule of
/// this function's own: a value that has text it reads back from contributes
/// that text, and a value that has none — an array, an object, bytes — is
/// refused. Sharing the rule matters because the language has no way to convert
/// the elements of an array one by one, so a stricter `join` accepting only text
/// would leave a caller holding `[1, 2]` with no sentence to write at all. That
/// is the same test `type::float` had to pass.
pub(crate) fn join(items: &[Value], separator: &str, span: Span) -> Result<Value> {
    let mut text = String::new();
    for (position, item) in items.iter().enumerate() {
        if position > 0 {
            text.push_str(separator);
        }
        let Value::String(held) = crate::cast::read(Function::TypeString, item, span)? else {
            return Err(Error::CallFailed {
                function: Function::ArrayJoin,
                reason: "an element has no text to join",
                span,
            });
        };
        text.push_str(&held);
    }
    Ok(Value::from(text.as_str()))
}

/// A run of `count` elements starting at `start`.
///
/// **A start past the end answers an empty array**, and a count reaching past it
/// takes what is there. Neither is a mistake: asking for ten elements from a
/// list that holds three is how paging through a shrinking list reads, and the
/// answer "there are none left" is the true one. A **negative** start or count is
/// refused, because it is not a short answer — it is a caller who meant
/// something this function does not do, and answering an empty array would hide
/// that.
pub(crate) fn slice(items: &[Value], start: i64, count: i64, span: Span) -> Result<Value> {
    let refused = |reason: &'static str| Error::CallFailed {
        function: Function::ArraySlice,
        reason,
        span,
    };
    if start < 0 {
        return Err(refused("a slice starts at or after the first element"));
    }
    if count < 0 {
        return Err(refused("a slice holds no fewer than no elements"));
    }
    let held = usize::try_from(start).map_or_else(
        |_| Vec::new(),
        |from| {
            items
                .iter()
                .skip(from)
                .take(usize::try_from(count).unwrap_or(usize::MAX))
                .cloned()
                .collect()
        },
    );
    Ok(Value::Array(held))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic)]

    use tessari_ql::Span;
    use tessari_types::{Number, Value};

    use super::{distinct, flatten, join, keys, reverse, slice, sort, values};

    fn at() -> Span {
        Span::new(0, 1)
    }

    fn number(held: i64) -> Value {
        Value::from(held)
    }

    fn array(items: Vec<Value>) -> Value {
        Value::Array(items)
    }

    #[test]
    fn the_names_and_the_values_of_an_object_correspond_position_by_position() {
        // The property that makes the pair usable: nothing in the language zips
        // two arrays, so a caller reading `object::keys` and `object::values`
        // separately has no way to pair them if these two disagree. Written in
        // a deliberately unsorted order, since the answer must not depend on it.
        let object: std::collections::BTreeMap<String, Value> = [
            ("zone".to_owned(), Value::from("north")),
            ("age".to_owned(), number(41)),
            ("name".to_owned(), Value::from("ada")),
        ]
        .into_iter()
        .collect();
        assert_eq!(
            keys(&object),
            array(vec![
                Value::from("age"),
                Value::from("name"),
                Value::from("zone")
            ])
        );
        assert_eq!(
            values(&object),
            array(vec![number(41), Value::from("ada"), Value::from("north")])
        );
    }

    #[test]
    fn sorting_uses_the_order_the_value_system_declares_and_not_a_second_one() {
        // Across types, so the answer matches what an index would give back.
        let mixed = [
            Value::from("b"),
            number(2),
            Value::from("a"),
            number(1),
            Value::Bool(true),
        ];
        let Value::Array(held) = sort(&mixed) else {
            panic!("an array");
        };
        let mut expected = mixed.to_vec();
        expected.sort();
        assert_eq!(held, expected);
        // And it is genuinely ordered, not merely permuted.
        assert!(held.windows(2).all(|pair| pair[0] <= pair[1]));
    }

    #[test]
    fn descending_is_reverse_of_sort_which_is_why_there_is_no_second_function() {
        let items = [number(3), number(1), number(2)];
        let Value::Array(ascending) = sort(&items) else {
            panic!("an array");
        };
        let Value::Array(descending) = reverse(&ascending) else {
            panic!("an array");
        };
        assert_eq!(descending, vec![number(3), number(2), number(1)]);
    }

    #[test]
    fn distinct_keeps_the_first_occurrence_so_the_given_order_survives() {
        let items = [number(3), number(1), number(3), number(2), number(1)];
        assert_eq!(
            distinct(&items),
            array(vec![number(3), number(1), number(2)])
        );
        // And the sorted form is still reachable, which is the reason this is
        // the behaviour and sorting-while-deduplicating is not.
        assert_eq!(
            sort(&[number(3), number(1), number(2)]),
            array(vec![number(1), number(2), number(3)])
        );
    }

    #[test]
    fn distinct_agrees_with_the_equality_the_rest_of_the_language_uses() {
        // `1`, `1.0` and `dec 1` are one value, exactly as they are one set
        // member. A second notion of sameness here would be a real defect.
        let items = [
            number(1),
            Value::Number(Number::float(1.0)),
            Value::from("1"),
        ];
        let Value::Array(held) = distinct(&items) else {
            panic!("an array");
        };
        assert_eq!(held.len(), 2, "{held:?}");
    }

    #[test]
    fn flatten_removes_one_level_and_keeps_what_was_never_nested() {
        let items = [
            array(vec![number(1), number(2)]),
            number(3),
            array(vec![array(vec![number(4)])]),
        ];
        assert_eq!(
            flatten(&items),
            array(vec![
                number(1),
                number(2),
                number(3),
                array(vec![number(4)])
            ]),
            "the doubly-nested element should have come out singly nested"
        );
    }

    #[test]
    fn joining_reads_each_element_the_way_a_cast_to_text_would() {
        let items = [number(1), Value::from("b"), Value::Bool(true)];
        assert_eq!(
            join(&items, ", ", at()).expect("text"),
            Value::from("1, b, true")
        );
        assert_eq!(join(&[], ",", at()).expect("text"), Value::from(""));
    }

    #[test]
    fn an_element_with_no_text_it_reads_back_from_is_refused() {
        // `<array of 2>` is a sentence about a value, and joining it would put
        // that sentence where the value belonged.
        let items = [number(1), array(vec![number(1), number(2)])];
        assert!(join(&items, ",", at()).is_err());
    }

    #[test]
    fn a_slice_past_the_end_is_empty_rather_than_a_failure() {
        let items = [number(1), number(2), number(3)];
        assert_eq!(
            slice(&items, 1, 2, at()).expect("a slice"),
            array(vec![number(2), number(3)])
        );
        // Asking for more than is left takes what is there — how paging through
        // a shrinking list reads.
        assert_eq!(
            slice(&items, 2, 10, at()).expect("a slice"),
            array(vec![number(3)])
        );
        assert_eq!(slice(&items, 9, 1, at()).expect("a slice"), array(vec![]));
        assert_eq!(slice(&items, 0, 0, at()).expect("a slice"), array(vec![]));
    }

    #[test]
    fn a_negative_bound_is_refused_rather_than_answered_with_an_empty_slice() {
        // An empty answer would hide a caller who meant "from the end" — which
        // this function does not do.
        let items = [number(1), number(2)];
        assert!(slice(&items, -1, 1, at()).is_err());
        assert!(slice(&items, 0, -1, at()).is_err());
    }
}
