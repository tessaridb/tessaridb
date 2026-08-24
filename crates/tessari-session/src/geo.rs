//! What the `geo::` functions do, once their arguments are values.
//!
//! # This module holds no geometry
//!
//! It converts two values into shapes on the grid and asks
//! [`tessari_geo`]. Every geometric decision — what a boundary counts as, when
//! `contains` and `covers` part company, how a segment inside a region is
//! decided without leaving the integers — lives in that crate and is tested
//! against fixtures there.
//!
//! The separation is deliberate rather than tidy. A geometric kernel is only
//! ever known to be right by being run against an oracle, and an oracle can only
//! be pointed at a layer that has no session, no store and no statement in it.
//!
//! # A missing field answers `none`; a wrong type is a mistake
//!
//! The absence rule one level up already answers `none` when an argument holds
//! nothing, which is what lets a read over records of differing shapes narrow
//! instead of failing. A value that is *present* and is not a geometry is a
//! different thing: it is an error in the statement, and reporting it as `none`
//! would drop rows without saying why.

use tessari_geo::Shape;
use tessari_ql::{Function, Span};
use tessari_types::Value;

use crate::error::{Error, Result};

/// Answer one of the `geo::` predicates about two shapes.
///
/// The predicate arrives as a function rather than being matched on here, so
/// that adding one is a line in the caller's exhaustive match and nothing at
/// all in this file.
///
/// # Errors
///
/// Returns [`Error::WrongArgument`] when either argument is present and is not a
/// geometry, and [`Error::GeometryRefused`] when a position is off the sphere —
/// a query shape is not stored, so it is not held to the store's validity rules,
/// but it is still held to being somewhere on the planet.
pub(crate) fn relate(
    function: Function,
    judge: fn(&Shape, &Shape) -> bool,
    arguments: &[Value],
    span: Span,
) -> Result<Value> {
    let one = shape_at(function, arguments, 0, span)?;
    let other = shape_at(function, arguments, 1, span)?;
    Ok(Value::Bool(judge(&one, &other)))
}

fn shape_at(function: Function, arguments: &[Value], at: usize, span: Span) -> Result<Shape> {
    match arguments.get(at) {
        Some(Value::Geometry(geometry)) => {
            Shape::of(geometry).map_err(|off_grid| Error::GeometryRefused {
                refused: off_grid.into(),
                span,
            })
        }
        other => Err(Error::WrongArgument {
            function,
            at: at.saturating_add(1),
            expected: "a geometry",
            found: other.map_or("nothing", Value::type_name),
            span,
        }),
    }
}
