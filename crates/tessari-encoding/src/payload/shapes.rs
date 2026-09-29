//! How a shape is written into a stored record and read back.

use super::{count_of, deeper, shape};
use crate::error::{Error, Result};
use crate::order::{KeyReader, KeyWriter};
use tessari_types::{Geometry, Polygon, Position, Ring};

/// A position: two floats, as their bits.
///
/// Bits rather than an order-preserving form, and that is right here: a payload
/// is read, never compared byte by byte. The index has its own encoding, and
/// keeping the two apart is what stops a payload from becoming a second
/// ordering authority.
pub(crate) fn put_position(writer: &mut KeyWriter, position: &Position) {
    writer
        .put_fixed(&position.longitude.to_bits().to_be_bytes())
        .put_fixed(&position.latitude.to_bits().to_be_bytes());
}

pub(crate) fn take_position(reader: &mut KeyReader<'_>) -> Result<Position> {
    let longitude = f64::from_bits(u64::from_be_bytes(reader.take_fixed::<8>()?));
    let latitude = f64::from_bits(u64::from_be_bytes(reader.take_fixed::<8>()?));
    Ok(Position::new(longitude, latitude))
}

pub(crate) fn put_positions(writer: &mut KeyWriter, positions: &[Position]) {
    writer.put_u32(count_of(positions.len()));
    for position in positions {
        put_position(writer, position);
    }
}

pub(crate) fn take_positions(reader: &mut KeyReader<'_>) -> Result<Vec<Position>> {
    let count = reader.take_u32()?;
    let mut out = Vec::new();
    for _ in 0..count {
        out.push(take_position(reader)?);
    }
    Ok(out)
}

pub(crate) fn put_polygon(writer: &mut KeyWriter, polygon: &Polygon) {
    put_positions(writer, &polygon.exterior.0);
    writer.put_u32(count_of(polygon.interiors.len()));
    for interior in &polygon.interiors {
        put_positions(writer, &interior.0);
    }
}

pub(crate) fn take_polygon(reader: &mut KeyReader<'_>) -> Result<Polygon> {
    let exterior = Ring(take_positions(reader)?);
    let count = reader.take_u32()?;
    let mut interiors = Vec::new();
    for _ in 0..count {
        interiors.push(Ring(take_positions(reader)?));
    }
    Ok(Polygon {
        exterior,
        interiors,
    })
}

pub(crate) fn put_geometry(writer: &mut KeyWriter, held: &Geometry) {
    match held {
        Geometry::Point(position) => {
            writer.put_u8(shape::POINT);
            put_position(writer, position);
        }
        Geometry::Line(positions) => {
            writer.put_u8(shape::LINE);
            put_positions(writer, positions);
        }
        Geometry::Polygon(polygon) => {
            writer.put_u8(shape::POLYGON);
            put_polygon(writer, polygon);
        }
        Geometry::MultiPoint(positions) => {
            writer.put_u8(shape::MULTI_POINT);
            put_positions(writer, positions);
        }
        Geometry::MultiLine(lines) => {
            writer
                .put_u8(shape::MULTI_LINE)
                .put_u32(count_of(lines.len()));
            for line in lines {
                put_positions(writer, line);
            }
        }
        Geometry::MultiPolygon(polygons) => {
            writer
                .put_u8(shape::MULTI_POLYGON)
                .put_u32(count_of(polygons.len()));
            for polygon in polygons {
                put_polygon(writer, polygon);
            }
        }
        Geometry::Collection(shapes) => {
            writer
                .put_u8(shape::COLLECTION)
                .put_u32(count_of(shapes.len()));
            for held in shapes {
                put_geometry(writer, held);
            }
        }
    }
}

pub(crate) fn take_geometry(reader: &mut KeyReader<'_>, depth: usize) -> Result<Geometry> {
    match reader.take_u8()? {
        shape::POINT => Ok(Geometry::Point(take_position(reader)?)),
        shape::LINE => Ok(Geometry::Line(take_positions(reader)?)),
        shape::POLYGON => Ok(Geometry::Polygon(take_polygon(reader)?)),
        shape::MULTI_POINT => Ok(Geometry::MultiPoint(take_positions(reader)?)),
        shape::MULTI_LINE => {
            let count = reader.take_u32()?;
            let mut lines = Vec::new();
            for _ in 0..count {
                lines.push(take_positions(reader)?);
            }
            Ok(Geometry::MultiLine(lines))
        }
        shape::MULTI_POLYGON => {
            let count = reader.take_u32()?;
            let mut polygons = Vec::new();
            for _ in 0..count {
                polygons.push(take_polygon(reader)?);
            }
            Ok(Geometry::MultiPolygon(polygons))
        }
        shape::COLLECTION => {
            let inside = deeper(depth)?;
            let count = reader.take_u32()?;
            let mut shapes = Vec::new();
            for _ in 0..count {
                shapes.push(Box::new(take_geometry(reader, inside)?));
            }
            Ok(Geometry::Collection(shapes))
        }
        unknown => Err(Error::UnknownValueTag { tag: unknown }),
    }
}
