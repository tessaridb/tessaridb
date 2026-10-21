//! The RESP protocol as a surface of a TessariDB node (ADR-0129).
//!
//! A RESP client points at the node and reads and writes the keys of one of
//! its spaces: a key is a record of the space, a value is what the record
//! holds, an expiry is the space's own, and a user is a store user. Nothing
//! runs beside the store.
//!
//! This crate holds the protocol in two halves. The codec — commands read
//! from arrays of bulk strings or an inline line, replies written in either
//! protocol version — is pure and synchronous. The surface that serves it runs
//! on the node's tokio runtime and turns every command into statements on the
//! caller's session.

#![forbid(unsafe_code)]
#![cfg_attr(
    test,
    allow(clippy::expect_used, clippy::unwrap_used, clippy::as_conversions)
)]

pub mod codec;
mod error;
mod surface;

pub use surface::{Node, Space};

pub use error::{Error, Result};
