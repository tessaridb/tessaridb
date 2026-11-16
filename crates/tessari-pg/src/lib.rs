//! The v3 relational wire protocol as a surface of a TessariDB node (ADR-0138).
//!
//! A wire-protocol client points at the node and reads the node's own tables
//! with the statements of its own SQL dialect: a table is a table, a row is a
//! record, a user is a store user, and what the dialect cannot say honestly is
//! refused by name rather than approximated. Nothing runs beside the store.
//!
//! This crate holds the protocol in two halves, as `tessari-resp`'s does. The
//! codec — startup and query messages read and written over a buffer — is pure
//! and synchronous. The surface that serves it runs on the node's tokio
//! runtime and turns every query into statements on the caller's session.

#![forbid(unsafe_code)]
#![cfg_attr(
    test,
    allow(clippy::expect_used, clippy::unwrap_used, clippy::as_conversions)
)]

pub mod codec;
mod error;
#[cfg(feature = "surface")]
mod surface;

#[cfg(feature = "surface")]
pub use surface::Node;

pub use error::{Error, Result};
