//! ApiVersions: the first request of every connection.

use crate::api::{API_VERSIONS, SERVED, served};
use crate::codec::Writer;
use crate::error::Result;

/// The protocol's error code for a version the node does not speak.
pub const UNSUPPORTED_VERSION: i16 = 35;

/// The answer to ApiVersions at `version`.
///
/// A version this node does not speak is answered at version 0 with
/// `UNSUPPORTED_VERSION` and the full table, which is the one exception the
/// protocol makes so a newer client can step down rather than disconnect.
///
/// # Errors
///
/// What the writer refuses.
pub fn answer(writer: &mut Writer, version: i16) -> Result<()> {
    let spoken = served(API_VERSIONS).is_some_and(|api| api.serves(version));
    let (version, error) = if spoken {
        (version, 0)
    } else {
        (0, UNSUPPORTED_VERSION)
    };
    let flexible = version >= 3;
    writer.i16(error);
    writer.array_len(flexible, Some(SERVED.len()))?;
    for api in SERVED {
        writer.i16(api.key);
        writer.i16(api.min);
        writer.i16(api.max);
        if flexible {
            writer.no_tagged_fields();
        }
    }
    if version >= 1 {
        writer.i32(0);
    }
    if flexible {
        writer.no_tagged_fields();
    }
    Ok(())
}
