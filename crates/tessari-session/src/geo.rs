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

use tessari_geo::{Shape, Snapped};
use tessari_ql::{Function, Span};
use tessari_types::{Number, Value};

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

/// How far apart two positions are, in metres along the ellipsoid.
///
/// **Positions only.** A distance between larger shapes is the distance to the
/// nearest part of them, which is a different computation and is not written
/// yet — so a larger shape is refused by name rather than answered about from
/// one of its corners, which is what a store that quietly used a representative
/// vertex would be doing.
///
/// Two positions on opposite sides of the world answer `none`: the solution
/// does not converge there, and the number it would otherwise return is wrong by
/// an amount nobody can bound.
///
/// # Errors
///
/// Returns [`Error::WrongArgument`] when either argument is not a position, and
/// [`Error::GeometryRefused`] when one is off the sphere.
pub(crate) fn separation(function: Function, arguments: &[Value], span: Span) -> Result<Value> {
    // An absence is unreachably far, not unknown — the same answer the vector
    // distances give and for the same reason: `NONE` sorts below every value, so
    // propagating it would make `ORDER BY … LIMIT 10` answer with exactly the
    // records that have no shape, in first place.
    //
    // Unlike the vector distances, a *present* value of the wrong type is still
    // a mistake here rather than an infinity. A caller who wrote a string where
    // a shape belongs has an error in the statement, and an infinity would sort
    // it quietly to the end instead of saying so.
    if arguments
        .iter()
        .any(|value| !value.is_present() || *value == Value::Null)
    {
        return Ok(Value::Number(Number::float(f64::INFINITY)));
    }
    let one = position_at(function, arguments, 0, span)?;
    let other = position_at(function, arguments, 1, span)?;
    Ok(tessari_geo::distance(one, other)
        .map_or(Value::None, |metres| Value::Number(Number::float(metres))))
}

/// How much ground a shape covers, in square metres.
///
/// # Errors
///
/// Returns [`Error::WrongArgument`] when the argument is not a geometry, and
/// [`Error::GeometryRefused`] when it is off the sphere.
pub(crate) fn ground(function: Function, arguments: &[Value], span: Span) -> Result<Value> {
    let shape = shape_at(function, arguments, 0, span)?;
    Ok(Value::Number(Number::float(tessari_geo::area(&shape))))
}

fn position_at(function: Function, arguments: &[Value], at: usize, span: Span) -> Result<Snapped> {
    match arguments.get(at) {
        Some(Value::Geometry(tessari_types::Geometry::Point(position))) => Snapped::of(*position)
            .map_err(|off_grid| Error::GeometryRefused {
                refused: off_grid.into(),
                span,
            }),
        Some(Value::Geometry(shape)) => Err(Error::WrongArgument {
            function,
            at: at.saturating_add(1),
            expected: "a position",
            found: shape.kind_name(),
            span,
        }),
        other => Err(Error::WrongArgument {
            function,
            at: at.saturating_add(1),
            expected: "a position",
            found: other.map_or("nothing", Value::type_name),
            span,
        }),
    }
}
