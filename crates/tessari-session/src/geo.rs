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

/// How far apart two shapes are, in metres along the ellipsoid, when at least
/// one of them is a position.
///
/// Between two positions it is the geodesic between them. Between a position and
/// a larger shape it is the distance to the shape's **nearest point** — zero
/// when the shape covers the position, by the same exact rule `geo::covers`
/// answers with — which [`tessari_geo::distance_to`] finds without ever
/// answering from a representative vertex.
///
/// Two shapes that are **both** larger than a position are refused by name: the
/// distance between them is the least over two sets of points, a different
/// search, and not written yet.
///
/// Two positions on opposite sides of the world answer `none`: the solution
/// does not converge there, and the number it would otherwise return is wrong by
/// an amount nobody can bound.
///
/// # Errors
///
/// Returns [`Error::WrongArgument`] when an argument is not a geometry or when
/// neither is a position, and [`Error::GeometryRefused`] when one is off the
/// sphere.
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
    let one = shape_at(function, arguments, 0, span)?;
    let other = shape_at(function, arguments, 1, span)?;
    let metres = match (&one, &other) {
        (Shape::Point(here), Shape::Point(there)) => tessari_geo::distance(*here, *there),
        (Shape::Point(here), shape) | (shape, Shape::Point(here)) => {
            tessari_geo::distance_to(*here, shape)
        }
        _ => {
            return Err(Error::WrongArgument {
                function,
                at: 2,
                expected: "a position (one of the two must be)",
                found: arguments.get(1).map_or("nothing", larger_name),
                span,
            });
        }
    };
    Ok(metres.map_or(Value::None, |metres| Value::Number(Number::float(metres))))
}

/// The kind a present geometry argument was written as.
fn larger_name(value: &Value) -> &'static str {
    match value {
        Value::Geometry(shape) => shape.kind_name(),
        other => other.type_name(),
    }
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

/// The spatial index's cell holding a position at one level, as a polygon.
///
/// A key to group by — `GROUP BY geo::cell(at, 8)` puts the records falling in
/// one cell together — and one that draws itself, since the answer is the cell's
/// own box rather than an identifier a client would need a second function to
/// turn back into a shape. The box is the one the index uses, so the cells of
/// one level tile the world with nothing counted twice.
///
/// Cells are not equal in area: they divide longitude and latitude evenly, so a
/// cell near a pole covers less ground than one at the equator. A density is
/// `count(*) / geo::area(geo::cell(at, n))`, and a count alone is for drawing.
///
/// # Errors
///
/// Returns [`Error::WrongArgument`] when the first argument is not a position or
/// the second is not a whole number from 0 to the finest level, and
/// [`Error::GeometryRefused`] when the position is off the sphere.
pub(crate) fn cell(function: Function, arguments: &[Value], span: Span) -> Result<Value> {
    let Shape::Point(position) = shape_at(function, arguments, 0, span)? else {
        return Err(Error::WrongArgument {
            function,
            at: 1,
            expected: "a position — an area or a path spans cells",
            found: arguments.first().map_or("nothing", larger_name),
            span,
        });
    };
    let level = match arguments.get(1) {
        Some(Value::Number(Number::Integer(level))) => u32::try_from(*level)
            .ok()
            .filter(|level| *level <= tessari_geo::ORDER),
        _ => None,
    };
    let Some(level) = level else {
        return Err(Error::WrongArgument {
            function,
            at: 2,
            expected: "a whole number of levels from 0 to 32",
            found: arguments.get(1).map_or("nothing", |value| match value {
                Value::Number(Number::Integer(_)) => "a number outside that range",
                other => other.type_name(),
            }),
            span,
        });
    };
    let square = tessari_geo::Cell::containing(position)
        .ancestor(level)
        .and_then(tessari_geo::Cell::extent)
        .and_then(|extent| {
            let low = tessari_geo::Snapped::from_units(extent.west(), extent.south()).ok()?;
            let high = tessari_geo::Snapped::from_units(extent.east(), extent.north()).ok()?;
            Some((low.to_position(), high.to_position()))
        });
    let Some((low, high)) = square else {
        // Every level up to the finest has an ancestor and every cell an extent
        // inside the grid; reaching here would mean the cell arithmetic is broken,
        // and an answer made up for it would be a wrong shape.
        return Ok(Value::None);
    };
    let corner = |longitude, latitude| tessari_types::Position::new(longitude, latitude);
    Ok(Value::Geometry(tessari_types::Geometry::Polygon(
        tessari_types::Polygon {
            exterior: tessari_types::Ring(vec![
                corner(low.longitude, low.latitude),
                corner(high.longitude, low.latitude),
                corner(high.longitude, high.latitude),
                corner(low.longitude, high.latitude),
                corner(low.longitude, low.latitude),
            ]),
            interiors: Vec::new(),
        },
    )))
}
