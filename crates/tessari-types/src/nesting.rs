//! How deep a value may nest, and the one question asked about it.
//!
//! Every reader of a value is recursive — the decoder, the encoder, the
//! evaluator, and `Drop` itself — so a value nested deeply enough ends the
//! process by running out of stack. That is not a panic and nothing catches
//! it: one crafted value would stop the node for everybody. So one ceiling
//! holds wherever a value can be made from outside: the decoder refuses to
//! read past it, and a write refuses to store past it, which is what keeps the
//! decoder's refusal from ever meeting a value the store accepted.

use std::ops::Bound;

use crate::{Geometry, Value};

/// How many containers a value may hold one inside another.
///
/// Counted in containers on the deepest path: `1` is not nested, `[1]` is one
/// level, `[[1]]` two. An array, an object, a set, a range and a geometry
/// collection each count as one. Chosen far past any real document and far
/// short of the stack: the point is not a realistic depth, it is that an
/// unbounded one is a crash. The stream reader's JSON ceiling is the same
/// number.
pub const MAX_NESTING: usize = 64;

impl Value {
    /// Whether this value holds containers more than `limit` deep.
    ///
    /// Stops at the first path past the limit rather than measuring the whole
    /// value, so the walk never recurses more than `limit` levels whatever the
    /// value holds.
    #[must_use]
    pub fn nests_deeper_than(&self, limit: usize) -> bool {
        let inner = match limit.checked_sub(1) {
            Some(inner) => inner,
            None => return self.is_container(),
        };
        match self {
            Self::Array(items) => items.iter().any(|item| item.nests_deeper_than(inner)),
            Self::Set(items) => items.iter().any(|item| item.nests_deeper_than(inner)),
            Self::Object(fields) => fields.values().any(|field| field.nests_deeper_than(inner)),
            Self::Range(range) => [&range.start, &range.end].into_iter().any(|end| match end {
                Bound::Included(held) | Bound::Excluded(held) => held.nests_deeper_than(inner),
                Bound::Unbounded => false,
            }),
            Self::Geometry(shape) => shape_deeper_than(shape, limit),
            _ => false,
        }
    }

    const fn is_container(&self) -> bool {
        matches!(
            self,
            Self::Array(_)
                | Self::Set(_)
                | Self::Object(_)
                | Self::Range(_)
                | Self::Geometry(Geometry::Collection(_))
        )
    }
}

/// The same question about a shape, whose only container is a collection.
fn shape_deeper_than(shape: &Geometry, limit: usize) -> bool {
    let Geometry::Collection(shapes) = shape else {
        return false;
    };
    match limit.checked_sub(1) {
        Some(inner) => shapes.iter().any(|held| shape_deeper_than(held, inner)),
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn arrays(depth: usize) -> Value {
        (0..depth).fold(Value::Null, |inner, _| Value::Array(vec![inner]))
    }

    #[test]
    fn a_value_is_as_deep_as_its_containers() {
        assert!(!Value::Null.nests_deeper_than(0));
        assert!(arrays(1).nests_deeper_than(0));
        assert!(!arrays(MAX_NESTING).nests_deeper_than(MAX_NESTING));
        assert!(arrays(MAX_NESTING.saturating_add(1)).nests_deeper_than(MAX_NESTING));
    }

    #[test]
    fn every_kind_of_container_counts_one_level() {
        let object = Value::Object(BTreeMap::from([("a".to_owned(), arrays(1))]));
        let set = Value::Set([arrays(1)].into_iter().collect());
        let range = Value::Range(Box::new(crate::ValueRange::new(
            Bound::Included(arrays(1)),
            Bound::Unbounded,
        )));
        let shapes = Value::Geometry(Geometry::Collection(vec![Box::new(Geometry::Collection(
            Vec::new(),
        ))]));
        for held in [object, set, range, shapes] {
            assert!(held.nests_deeper_than(1), "{held:?} is two levels deep");
            assert!(
                !held.nests_deeper_than(2),
                "{held:?} is only two levels deep"
            );
        }
    }
}
