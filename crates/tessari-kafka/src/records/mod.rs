//! Record batches: decoded from a producer, encoded for a fetch.

mod batch;
mod compression;
mod crc32;

pub use batch::{Batch, Header, Outgoing, OutgoingHeader, Record, decode_batches, encode_batch};
pub use compression::{Codec, DECOMPRESSED_CEILING};

#[cfg(test)]
mod tests;
