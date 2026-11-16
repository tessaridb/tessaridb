//! The wire protocol's messages, in both directions.
//!
//! Reading and writing are pure functions over bytes; the surface owns the
//! sockets. Every length is checked against [`CEILING`] before anything is
//! allocated, in both directions — a peer that emits what it would refuse to
//! read is running two protocols.

pub(crate) mod read;
pub(crate) mod write;

pub use read::{First, Frontend};
pub use write::{Column, Tx};

/// The request code that opens a TLS negotiation instead of a session.
pub(crate) const SSL_REQUEST: u32 = 80877103;

/// The request code that asks a running query to stop, answered by closing.
pub(crate) const CANCEL_REQUEST: u32 = 80877102;

/// The protocol version this surface speaks: 3.0, the shape every client of
/// the dialect understands. A newer offer is answered with this one, which the
/// version negotiation of the protocol makes legal.
pub(crate) const PROTOCOL_VERSION: u32 = 3 << 16;

#[cfg(test)]
mod tests;
