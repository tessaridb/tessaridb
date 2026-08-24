//! Putting every shape in a record on the grid before the record is written.
//!
//! # Why a walk rather than a field rule
//!
//! A geometry is a value, and a value can be anywhere a value can be: at the top
//! of a record, in a field, in an array in a field, in an object in an array. A
//! check that ran only where a field was declared `TYPE geometry` would hold for
//! the schema-full table and miss the schemaless one, which is the store's
//! default and the shape most geometry will arrive in. Validity is a property of
//! the value, not of a declaration about it, so the walk visits values.
//!
//! # What the walk does to a shape
//!
//! It hands it to the geometry crate's boundary, which snaps it to the grid and
//! then judges the snapped result — in that order, because snapping can create
//! invalidity and judging first would accept exactly the shapes that order exists
//! to catch. What comes back is the shape as it will be stored, so the value the
//! record carries is the value the caller can read back unchanged.
//!
//! # What it does to a set
//!
//! A `SET` holds no duplicates, and two shapes distinct as written can be one
//! shape once snapped. Rebuilding the set from the snapped values therefore
//! collapses them, and the set the store keeps is shorter than the one that was
//! sent. That is the same fact as the precision model, arriving where it is
//! least expected, and it is why the set is rebuilt rather than mapped in place.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Bound;

use tessari_ql::Span;
use tessari_types::{Value, ValueRange};

use crate::error::{Error, Result};

/// Put every shape in a value on the grid, or refuse the write.
///
/// Almost every record written to this store contains no geometry at all, and
/// this sits on the path all of them take. So the value is **looked at** before
/// it is rebuilt: the scan borrows and allocates nothing, and the rebuilding
/// walk below — which re-collects every map, array and set it passes through —
/// runs only for the records that actually carry a shape. Without the scan, a
/// twenty-field record with no geometry in it would pay a full map rebuild on
/// every write to buy nothing.
///
/// # Errors
///
/// Returns [`Error::GeometryRefused`] naming the defect, its place in the shape,
/// and the **snapped** coordinates it concerns.
pub(crate) fn on_the_grid(value: Value, span: Span) -> Result<Value> {
    if !holds_a_shape(&value) {
        return Ok(value);
    }
    rebuilt_on_the_grid(value, span)
}

/// Whether a shape is anywhere inside this value.
///
/// Borrowing throughout, on purpose: this is the question the write path asks
/// every time, and the answer is almost always no.
fn holds_a_shape(value: &Value) -> bool {
    match value {
        Value::Geometry(_) => true,
        Value::Array(items) => items.iter().any(holds_a_shape),
        Value::Object(fields) => fields.values().any(holds_a_shape),
        Value::Set(members) => members.iter().any(holds_a_shape),
        Value::Range(span_of_values) => {
            bound_holds_a_shape(&span_of_values.start) || bound_holds_a_shape(&span_of_values.end)
        }
        _ => false,
    }
}

fn bound_holds_a_shape(bound: &Bound<Value>) -> bool {
    match bound {
        Bound::Included(value) | Bound::Excluded(value) => holds_a_shape(value),
        Bound::Unbounded => false,
    }
}

fn rebuilt_on_the_grid(value: Value, span: Span) -> Result<Value> {
    Ok(match value {
        Value::Geometry(shape) => Value::Geometry(
            tessari_geo::accept(&shape)
                .map_err(|refused| Error::GeometryRefused { refused, span })?,
        ),
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(|item| rebuilt_on_the_grid(item, span))
                .collect::<Result<Vec<_>>>()?,
        ),
        Value::Object(fields) => Value::Object(
            fields
                .into_iter()
                .map(|(name, held)| rebuilt_on_the_grid(held, span).map(|held| (name, held)))
                .collect::<Result<BTreeMap<_, _>>>()?,
        ),
        Value::Set(members) => Value::Set(
            members
                .into_iter()
                .map(|member| rebuilt_on_the_grid(member, span))
                .collect::<Result<BTreeSet<_>>>()?,
        ),
        Value::Range(span_of_values) => {
            let ValueRange { start, end } = *span_of_values;
            Value::Range(Box::new(ValueRange::new(
                bound_on_the_grid(start, span)?,
                bound_on_the_grid(end, span)?,
            )))
        }
        held => held,
    })
}

fn bound_on_the_grid(bound: Bound<Value>, span: Span) -> Result<Bound<Value>> {
    Ok(match bound {
        Bound::Included(value) => Bound::Included(rebuilt_on_the_grid(value, span)?),
        Bound::Excluded(value) => Bound::Excluded(rebuilt_on_the_grid(value, span)?),
        Bound::Unbounded => Bound::Unbounded,
    })
}
