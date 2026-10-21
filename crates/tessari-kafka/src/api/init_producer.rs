//! InitProducerId: an idempotent producer's identity (ADR-0129, D8 of the design).

use crate::codec::{Reader, Writer};
use crate::error::Result;

/// The protocol's code for a transactional id, which this node does not serve.
pub const TRANSACTIONAL_ID_AUTHORIZATION_FAILED: i16 = 53;

/// Read the request: only whether it names a transactional id matters.
///
/// # Errors
///
/// What the reader refuses.
pub fn read_request(reader: &mut Reader<'_>, version: i16, flexible: bool) -> Result<bool> {
    let transactional = reader
        .nullable_string(flexible, "transactional_id")?
        .is_some();
    let _timeout = reader.i32("transaction_timeout_ms")?;
    if version >= 3 {
        let _id = reader.i64("producer_id")?;
        let _epoch = reader.i16("producer_epoch")?;
    }
    if flexible {
        reader.tagged_fields("init producer id")?;
    }
    Ok(transactional)
}

/// Write the answer.
pub fn write_response(
    writer: &mut Writer,
    flexible: bool,
    error_code: i16,
    producer_id: i64,
    epoch: i16,
) {
    writer.i32(0);
    writer.i16(error_code);
    writer.i64(producer_id);
    writer.i16(epoch);
    if flexible {
        writer.no_tagged_fields();
    }
}
