//! How a shape is written into, and skipped over in, an index key.

use super::{SEQUENCE_END, SEQUENCE_MORE, orderable, skip_sequence};
use crate::error::Result;
use crate::order::{KeyReader, KeyWriter};
use tessari_types::{Geometry, Polygon, Position, Ring};

pub(crate) fn put_position(writer: &mut KeyWriter, position: &Position) {
    writer
        .put_u64(orderable(position.longitude))
        .put_u64(orderable(position.latitude));
}

pub(crate) fn put_positions(writer: &mut KeyWriter, positions: &[Position]) {
    for position in positions {
        writer.put_u8(SEQUENCE_MORE);
        put_position(writer, position);
    }
    writer.put_u8(SEQUENCE_END);
}

pub(crate) fn put_ring(writer: &mut KeyWriter, ring: &Ring) {
    put_positions(writer, &ring.0);
}

pub(crate) fn put_polygon(writer: &mut KeyWriter, polygon: &Polygon) {
    put_ring(writer, &polygon.exterior);
    for interior in &polygon.interiors {
        writer.put_u8(SEQUENCE_MORE);
        put_ring(writer, interior);
    }
    writer.put_u8(SEQUENCE_END);
}

/// A geometry in its order-preserving form.
///
/// The shape's discriminant leads, because `Geometry`'s derived `Ord` compares
/// variants before contents. Everything after it is written in declaration
/// order, for the same reason.
pub(crate) fn put_geometry(writer: &mut KeyWriter, held: &Geometry) {
    match held {
        Geometry::Point(position) => {
            writer.put_u8(0);
            put_position(writer, position);
        }
        Geometry::Line(positions) => {
            writer.put_u8(1);
            put_positions(writer, positions);
        }
        Geometry::Polygon(polygon) => {
            writer.put_u8(2);
            put_polygon(writer, polygon);
        }
        Geometry::MultiPoint(positions) => {
            writer.put_u8(3);
            put_positions(writer, positions);
        }
        Geometry::MultiLine(lines) => {
            writer.put_u8(4);
            for line in lines {
                writer.put_u8(SEQUENCE_MORE);
                put_positions(writer, line);
            }
            writer.put_u8(SEQUENCE_END);
        }
        Geometry::MultiPolygon(polygons) => {
            writer.put_u8(5);
            for polygon in polygons {
                writer.put_u8(SEQUENCE_MORE);
                put_polygon(writer, polygon);
            }
            writer.put_u8(SEQUENCE_END);
        }
        Geometry::Collection(shapes) => {
            writer.put_u8(6);
            for shape in shapes {
                writer.put_u8(SEQUENCE_MORE);
                put_geometry(writer, shape);
            }
            writer.put_u8(SEQUENCE_END);
        }
    }
}

pub(crate) fn skip_position(reader: &mut KeyReader<'_>) -> Result<()> {
    reader.take_u64()?;
    reader.take_u64().map(|_| ())
}

pub(crate) fn skip_positions(reader: &mut KeyReader<'_>) -> Result<()> {
    skip_sequence(reader, skip_position)
}

pub(crate) fn skip_polygon(reader: &mut KeyReader<'_>) -> Result<()> {
    skip_positions(reader)?;
    skip_sequence(reader, skip_positions)
}

/// Step over a geometry's ordering form.
///
/// The shape byte here is the discriminant [`put_geometry`] writes, not the
/// payload codec's — the two encodings are separate and neither reads the
/// other's bytes.
pub(crate) fn skip_geometry(reader: &mut KeyReader<'_>) -> Result<()> {
    match reader.take_u8()? {
        0 => skip_position(reader),
        1 | 3 => skip_positions(reader),
        2 => skip_polygon(reader),
        4 => skip_sequence(reader, skip_positions),
        5 => skip_sequence(reader, skip_polygon),
        6 => skip_sequence(reader, skip_geometry),
        other => Err(crate::error::Error::UnknownIndexTag {
            kind: reader.kind(),
            tag: other,
        }),
    }
}
