//! Geometry as RFC 7946 shapes it, in both directions.
//!
//! # Why the two directions live in one file
//!
//! Because they are inverses, and inverses that live apart drift. The names are
//! the whole risk: `LineString` against `line`, `MultiLineString` against
//! `multiline` — a reader and a writer that disagreed about one of those would
//! produce a literal the parser refuses, or worse, a literal that parses as a
//! different shape. Keeping both here means the strings appear once each, next
//! to each other, and a round-trip test over all seven shapes closes the loop.
//!
//! # Why this is not a JSON codec
//!
//! It converts between [`Geometry`] and the store's own [`Value`], not between
//! geometry and text. Two callers need it and they need different text: the HTTP
//! surface writes JSON, and the language writes a TessariQL literal. Both are
//! renderings of the same object, so the object is what this file produces.
//!
//! # What it will not do
//!
//! **No repairs, no defaults and no coercions.** A missing `coordinates`, a
//! string where a number belongs, a position of one number or three — each is
//! named and refused. RFC 7946 allows a third element for altitude and this
//! store is two-dimensional (ADR-0026 D1), so a third element is refused rather
//! than dropped: silently discarding a caller's altitude would store a shape
//! they did not write.

use crate::geometry::{Geometry, Polygon, Position, Ring};
use crate::number::Number;
use crate::value::Value;

/// The name RFC 7946 gives this shape.
#[must_use]
pub const fn geojson_name(shape: &Geometry) -> &'static str {
    match shape {
        Geometry::Point(_) => "Point",
        Geometry::Line(_) => "LineString",
        Geometry::Polygon(_) => "Polygon",
        Geometry::MultiPoint(_) => "MultiPoint",
        Geometry::MultiLine(_) => "MultiLineString",
        Geometry::MultiPolygon(_) => "MultiPolygon",
        Geometry::Collection(_) => "GeometryCollection",
    }
}

/// Why a value is not a shape.
///
/// Every variant names the place as well as the fault, because a caller reading
/// "not a position" about a multi-polygon has three levels of nesting to search.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum Malformed {
    /// The value is not an object at all.
    #[error("a shape is written as an object with a `type` and `coordinates`")]
    NotAnObject,
    /// There is no `type`, or it does not hold text.
    #[error("a shape needs a `type` holding one of the seven shape names")]
    NoType,
    /// The `type` is text and is not one of the seven.
    #[error("{found:?} is not a shape name")]
    UnknownType {
        /// What was written.
        found: String,
    },
    /// The field a shape of this kind needs is missing.
    #[error("a {kind} needs a `{field}`")]
    NoField {
        /// The shape's name.
        kind: &'static str,
        /// The field it wanted.
        field: &'static str,
    },
    /// A coordinates array is nested to the wrong depth.
    #[error("a {kind} needs {wanted}")]
    Shaped {
        /// The shape's name.
        kind: &'static str,
        /// What the coordinates should have looked like.
        wanted: &'static str,
    },
    /// A position is not two numbers.
    #[error(
        "a position is two numbers, longitude first{}",
        if *had == 3 { " — this store holds no altitude" } else { "" }
    )]
    NotAPosition {
        /// How many elements it had.
        had: usize,
    },
    /// A coordinate is not a number this store can place.
    #[error("a coordinate is an integer or a float, and this one is {found}")]
    NotACoordinate {
        /// The type that was there.
        found: &'static str,
    },
}

/// Build a shape from the object RFC 7946 describes.
///
/// # Errors
///
/// Returns [`Malformed`] naming the fault and the shape it was reading.
pub fn from_geojson(value: &Value) -> Result<Geometry, Malformed> {
    // A shape that is already a shape. Not a coercion — the identity — and it is
    // here for the member of a collection written with its own marker, which is
    // redundant and is what a person writes. The canonical form this file emits
    // uses plain objects throughout, as RFC 7946 does.
    if let Value::Geometry(shape) = value {
        return Ok(shape.clone());
    }
    let Value::Object(fields) = value else {
        return Err(Malformed::NotAnObject);
    };
    let Some(Value::String(kind)) = fields.get("type") else {
        return Err(Malformed::NoType);
    };

    if kind == "GeometryCollection" {
        let Some(Value::Array(members)) = fields.get("geometries") else {
            return Err(Malformed::NoField {
                kind: "GeometryCollection",
                field: "geometries",
            });
        };
        return members
            .iter()
            .map(|member| from_geojson(member).map(Box::new))
            .collect::<Result<_, _>>()
            .map(Geometry::Collection);
    }

    let kind: &'static str = match kind.as_str() {
        "Point" => "Point",
        "LineString" => "LineString",
        "Polygon" => "Polygon",
        "MultiPoint" => "MultiPoint",
        "MultiLineString" => "MultiLineString",
        "MultiPolygon" => "MultiPolygon",
        other => {
            return Err(Malformed::UnknownType {
                found: other.to_owned(),
            });
        }
    };
    let Some(coordinates) = fields.get("coordinates") else {
        return Err(Malformed::NoField {
            kind,
            field: "coordinates",
        });
    };

    match kind {
        "Point" => position(coordinates).map(Geometry::Point),
        "LineString" => path(coordinates, kind, "an array of positions").map(Geometry::Line),
        "MultiPoint" => path(coordinates, kind, "an array of positions").map(Geometry::MultiPoint),
        "Polygon" => {
            rings(coordinates, kind).map(|interiors| Geometry::Polygon(polygon(interiors)))
        }
        "MultiLineString" => {
            let Value::Array(paths) = coordinates else {
                return Err(shaped(kind, "an array of arrays of positions"));
            };
            paths
                .iter()
                .map(|one| path(one, kind, "an array of arrays of positions"))
                .collect::<Result<_, _>>()
                .map(Geometry::MultiLine)
        }
        _ => {
            let Value::Array(areas) = coordinates else {
                return Err(shaped(kind, "an array of polygons"));
            };
            areas
                .iter()
                .map(|one| rings(one, kind).map(polygon))
                .collect::<Result<_, _>>()
                .map(Geometry::MultiPolygon)
        }
    }
}

/// Render a shape as the object RFC 7946 describes.
#[must_use]
pub fn to_geojson(shape: &Geometry) -> Value {
    let mut fields = std::collections::BTreeMap::new();
    fields.insert(
        "type".to_owned(),
        Value::String(geojson_name(shape).to_owned()),
    );
    match shape {
        Geometry::Collection(members) => {
            fields.insert(
                "geometries".to_owned(),
                Value::Array(members.iter().map(|member| to_geojson(member)).collect()),
            );
        }
        Geometry::Point(at) => {
            fields.insert("coordinates".to_owned(), written(*at));
        }
        Geometry::Line(positions) | Geometry::MultiPoint(positions) => {
            fields.insert("coordinates".to_owned(), written_all(positions));
        }
        Geometry::MultiLine(paths) => {
            fields.insert(
                "coordinates".to_owned(),
                Value::Array(paths.iter().map(|one| written_all(one)).collect()),
            );
        }
        Geometry::Polygon(area) => {
            fields.insert("coordinates".to_owned(), written_rings(area));
        }
        Geometry::MultiPolygon(areas) => {
            fields.insert(
                "coordinates".to_owned(),
                Value::Array(areas.iter().map(written_rings).collect()),
            );
        }
    }
    Value::Object(fields)
}

fn polygon(mut rings: Vec<Ring>) -> Polygon {
    // RFC 7946: the first ring is the shell and the rest are holes. An empty
    // list gives an empty shell rather than failing here — the ingest boundary
    // is what decides whether a ring bounds anything, and it says so far better
    // than a parser could.
    let exterior = if rings.is_empty() {
        Ring(Vec::new())
    } else {
        rings.remove(0)
    };
    Polygon {
        exterior,
        interiors: rings,
    }
}

fn rings(value: &Value, kind: &'static str) -> Result<Vec<Ring>, Malformed> {
    let Value::Array(items) = value else {
        return Err(shaped(kind, "an array of rings"));
    };
    items
        .iter()
        .map(|one| path(one, kind, "an array of rings").map(Ring))
        .collect()
}

fn path(
    value: &Value,
    kind: &'static str,
    wanted: &'static str,
) -> Result<Vec<Position>, Malformed> {
    let Value::Array(items) = value else {
        return Err(shaped(kind, wanted));
    };
    items.iter().map(position).collect()
}

fn position(value: &Value) -> Result<Position, Malformed> {
    let Value::Array(pair) = value else {
        return Err(Malformed::NotAPosition { had: 0 });
    };
    let [longitude, latitude] = pair.as_slice() else {
        return Err(Malformed::NotAPosition { had: pair.len() });
    };
    Ok(Position::new(coordinate(longitude)?, coordinate(latitude)?))
}

fn coordinate(value: &Value) -> Result<f64, Malformed> {
    match value {
        Value::Number(Number::Integer(whole)) =>
        {
            #[expect(
                clippy::cast_precision_loss,
                reason = "a coordinate outside 2^53 is outside the sphere and is refused downstream"
            )]
            Ok(*whole as f64)
        }
        Value::Number(Number::Float(held)) => Ok(*held),
        other => Err(Malformed::NotACoordinate {
            found: other.type_name(),
        }),
    }
}

const fn shaped(kind: &'static str, wanted: &'static str) -> Malformed {
    Malformed::Shaped { kind, wanted }
}

fn written(at: Position) -> Value {
    Value::Array(vec![
        Value::Number(Number::float(at.longitude)),
        Value::Number(Number::float(at.latitude)),
    ])
}

fn written_all(positions: &[Position]) -> Value {
    Value::Array(positions.iter().map(|at| written(*at)).collect())
}

fn written_rings(area: &Polygon) -> Value {
    let mut rings = vec![written_all(&area.exterior.0)];
    rings.extend(area.interiors.iter().map(|hole| written_all(&hole.0)));
    Value::Array(rings)
}

#[cfg(test)]
mod tests {
    use super::{Malformed, from_geojson, geojson_name, to_geojson};
    use crate::geometry::{Geometry, Polygon, Position, Ring};

    fn at(longitude: f64, latitude: f64) -> Position {
        Position::new(longitude, latitude)
    }

    fn ring(corners: &[(f64, f64)]) -> Ring {
        Ring(
            corners
                .iter()
                .map(|&(longitude, latitude)| at(longitude, latitude))
                .collect(),
        )
    }

    /// One of every shape, each with enough structure that a level of nesting
    /// lost on the way out or back would change it.
    fn every_shape() -> Vec<Geometry> {
        let square = Polygon {
            exterior: ring(&[(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0), (0.0, 0.0)]),
            interiors: vec![ring(&[
                (0.2, 0.2),
                (0.4, 0.2),
                (0.4, 0.4),
                (0.2, 0.4),
                (0.2, 0.2),
            ])],
        };
        vec![
            Geometry::Point(at(2.35, 48.85)),
            Geometry::Line(vec![at(0.0, 0.0), at(1.0, 2.0), at(3.0, -4.5)]),
            Geometry::Polygon(square.clone()),
            Geometry::MultiPoint(vec![at(1.0, 1.0), at(-2.0, -2.0)]),
            Geometry::MultiLine(vec![
                vec![at(0.0, 0.0), at(1.0, 1.0)],
                vec![at(5.0, 5.0), at(6.0, 6.0), at(7.0, 5.0)],
            ]),
            Geometry::MultiPolygon(vec![square.clone(), square]),
            Geometry::Collection(vec![
                Box::new(Geometry::Point(at(9.0, 9.0))),
                Box::new(Geometry::MultiPoint(vec![at(1.0, 1.0)])),
            ]),
        ]
    }

    #[test]
    fn every_shape_survives_the_round_trip_unchanged() {
        // The whole reason both directions live in one file. A reader and a
        // writer that disagreed about one name — `LineString` against `line` —
        // would produce a shape that parses as a different shape, and the two
        // would look correct read separately.
        for shape in every_shape() {
            let written = to_geojson(&shape);
            let read = from_geojson(&written);
            assert_eq!(read.as_ref(), Ok(&shape), "written as {written:?}");
        }
    }

    #[test]
    fn a_collection_nests_and_still_returns() {
        let nested = Geometry::Collection(vec![Box::new(Geometry::Collection(vec![Box::new(
            Geometry::Point(at(1.0, 2.0)),
        )]))]);
        assert_eq!(from_geojson(&to_geojson(&nested)), Ok(nested));
    }

    #[test]
    fn the_seven_names_are_the_ones_rfc_7946_uses() {
        let names: Vec<&str> = every_shape().iter().map(geojson_name).collect();
        assert_eq!(
            names,
            vec![
                "Point",
                "LineString",
                "Polygon",
                "MultiPoint",
                "MultiLineString",
                "MultiPolygon",
                "GeometryCollection"
            ]
        );
    }

    #[test]
    fn a_third_coordinate_is_refused_rather_than_dropped() {
        // RFC 7946 allows an altitude and this store is two-dimensional. Dropping
        // the third number would store a shape the caller did not write.
        let written = crate::value::Value::Object(
            [
                (
                    "type".to_owned(),
                    crate::value::Value::String("Point".to_owned()),
                ),
                (
                    "coordinates".to_owned(),
                    crate::value::Value::Array(vec![
                        crate::value::Value::from(1_i64),
                        crate::value::Value::from(2_i64),
                        crate::value::Value::from(3_i64),
                    ]),
                ),
            ]
            .into_iter()
            .collect(),
        );
        assert_eq!(
            from_geojson(&written),
            Err(Malformed::NotAPosition { had: 3 })
        );
    }

    #[test]
    fn a_name_that_is_not_a_shape_names_itself_in_the_refusal() {
        let written = crate::value::Value::Object(
            [(
                "type".to_owned(),
                crate::value::Value::String("Circle".to_owned()),
            )]
            .into_iter()
            .collect(),
        );
        assert_eq!(
            from_geojson(&written),
            Err(Malformed::UnknownType {
                found: "Circle".to_owned()
            })
        );
    }

    #[test]
    fn a_shape_with_no_coordinates_says_which_field_it_wanted() {
        let written = crate::value::Value::Object(
            [(
                "type".to_owned(),
                crate::value::Value::String("Polygon".to_owned()),
            )]
            .into_iter()
            .collect(),
        );
        assert_eq!(
            from_geojson(&written),
            Err(Malformed::NoField {
                kind: "Polygon",
                field: "coordinates"
            })
        );
    }
}
