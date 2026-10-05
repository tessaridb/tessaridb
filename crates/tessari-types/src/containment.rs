//! The (path, leaf) pairs a document holds — what a containment index writes for
//! a record and what a containment read asks it for (ADR-0116 D4).
//!
//! One function serves both sides, which is what makes the index a narrowing
//! device rather than a second answer: every record whose field contains the
//! document asked for holds **every** pair that document holds, so the records
//! holding all of them are a superset of the answer, and the read re-tests each
//! one against the whole condition.
//!
//! Why that superset holds, rule by rule of `CONTAINS` (`condition.rs`):
//!
//! - a field asked for must be there and contain its value, so its pairs are the
//!   held field's pairs under the same name;
//! - an array element asked for is contained by **some** element held, so its
//!   pairs appear under the same path, where every element is the step `[*]`
//!   (written `NULL`) — position is not part of a path, because order is not part
//!   of the rule;
//! - a leaf is contained when it is equal, and the index encodes equal values to
//!   equal bytes (`1`, `1.0` and `dec 1.0` included), as `=` compares them.
//!
//! A field asked for as `NONE` is not asked for, so it writes no pair; an empty
//! document or array asks for nothing, so it writes none either — and a read
//! whose document holds no pair at all keeps the scan.
//!
//! **A field holding an array is membership, not containment**, and membership
//! is exact: an element equal to the document asked for is, in particular, one
//! that contains it. So for an array at the top the record writes each element's
//! pairs as if it were the document itself, and the re-test keeps only the
//! elements that are equal.

use crate::Value;

/// One step of a path into a document: a field, or every element of an array.
const EVERY_ELEMENT: Value = Value::Null;

/// The pairs a record's indexed field holds, each as `[path, leaf]`.
///
/// A document writes its own pairs; an array or a set writes each element's, as
/// if each were the document (see the module note); anything else writes none,
/// since a single value contains no document.
#[must_use]
pub fn held_pairs(field: &Value) -> Vec<[Value; 2]> {
    let mut out = Vec::new();
    match field {
        Value::Object(_) => walk(field, &mut Vec::new(), &mut out),
        Value::Array(items) => {
            for item in items {
                walk_root(item, &mut out);
            }
        }
        Value::Set(items) => {
            for item in items {
                walk_root(item, &mut out);
            }
        }
        _ => {}
    }
    out.sort();
    out.dedup();
    out
}

/// The pairs a document asked for holds, each as `[path, leaf]` — empty when it
/// asks for nothing an index can narrow by, or is not a document at all.
#[must_use]
pub fn asked_pairs(asked: &Value) -> Vec<[Value; 2]> {
    let mut out = Vec::new();
    if matches!(asked, Value::Object(_)) {
        walk(asked, &mut Vec::new(), &mut out);
    }
    out.sort();
    out.dedup();
    out
}

/// An array element taken as a document of its own.
fn walk_root(item: &Value, out: &mut Vec<[Value; 2]>) {
    if matches!(item, Value::Object(_)) {
        walk(item, &mut Vec::new(), out);
    }
}

/// Every leaf under `value`, at `path`.
fn walk(value: &Value, path: &mut Vec<Value>, out: &mut Vec<[Value; 2]>) {
    match value {
        Value::Object(fields) => {
            for (name, held) in fields {
                path.push(Value::from(name.as_str()));
                walk(held, path, out);
                path.pop();
            }
        }
        Value::Array(items) => {
            path.push(EVERY_ELEMENT);
            for item in items {
                walk(item, path, out);
            }
            path.pop();
        }
        Value::Set(items) => {
            path.push(EVERY_ELEMENT);
            for item in items {
                walk(item, path, out);
            }
            path.pop();
        }
        Value::None => {}
        leaf => out.push([Value::Array(path.clone()), leaf.clone()]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BinaryOp, Number, apply};

    fn int(value: i64) -> Value {
        Value::Number(Number::Integer(value))
    }

    fn document(fields: &[(&str, Value)]) -> Value {
        Value::Object(
            fields
                .iter()
                .map(|(name, value)| ((*name).to_owned(), value.clone()))
                .collect(),
        )
    }

    #[test]
    fn a_document_holds_one_pair_per_leaf_with_arrays_as_every_element() {
        let held = document(&[
            ("a", int(1)),
            (
                "lines",
                Value::Array(vec![document(&[("sku", Value::from("b"))]), int(2)]),
            ),
            ("gone", Value::None),
        ]);
        assert_eq!(
            held_pairs(&held),
            vec![
                [Value::Array(vec![Value::from("a")]), int(1)],
                [
                    Value::Array(vec![Value::from("lines"), Value::Null]),
                    int(2)
                ],
                [
                    Value::Array(vec![Value::from("lines"), Value::Null, Value::from("sku")]),
                    Value::from("b")
                ],
            ]
        );
    }

    #[test]
    fn every_pair_asked_for_is_held_by_a_document_that_contains_it() {
        // The property the index rests on, over the shapes the rule names.
        let held = document(&[
            ("a", int(1)),
            ("b", document(&[("c", int(2)), ("d", Value::Null)])),
            (
                "tags",
                Value::Array(vec![Value::from("x"), Value::from("y")]),
            ),
            (
                "items",
                Value::Array(vec![document(&[("id", int(1)), ("q", int(3))])]),
            ),
        ]);
        let questions = [
            document(&[("b", document(&[("c", int(2))]))]),
            document(&[("b", document(&[("d", Value::Null)]))]),
            document(&[("tags", Value::Array(vec![Value::from("y")]))]),
            document(&[("items", Value::Array(vec![document(&[("id", int(1))])]))]),
            document(&[("a", Value::Number(Number::Float(1.0)))]),
        ];
        let pairs = held_pairs(&held);
        for asked in &questions {
            assert!(apply(BinaryOp::Contains, &held, asked), "{asked:?}");
            for pair in asked_pairs(asked) {
                assert!(
                    pairs
                        .iter()
                        .any(|held| held[0] == pair[0] && held[1] == pair[1]),
                    "{pair:?} asked by {asked:?} is not held"
                );
            }
        }
    }

    #[test]
    fn an_array_at_the_top_holds_each_element_as_a_document() {
        let held = Value::Array(vec![document(&[("a", int(1))]), int(5)]);
        assert_eq!(
            held_pairs(&held),
            vec![[Value::Array(vec![Value::from("a")]), int(1)]]
        );
        assert!(asked_pairs(&int(1)).is_empty());
        assert!(asked_pairs(&document(&[])).is_empty());
    }
}
