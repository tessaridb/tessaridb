//! Reading a message as JSON.
//!
//! The reader lives in `tessari_types::json`, beside the writer, so a stream
//! message, a script's `json::parse` and the HTTP surface read and write one
//! mapping (ADR-0116 D1). Its tests stay here, where it was first written.

pub use tessari_types::json::read;

#[cfg(test)]
mod tests;
