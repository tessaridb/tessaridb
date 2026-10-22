//! The Kafka protocol as a surface of a TessariDB node (ADR-0129).
//!
//! A Kafka client points its bootstrap address at the node and produces to and
//! fetches from the node's own topics: a record lands as a message of the
//! topic, a consumer group's committed offset is the topic's named reader
//! position, and a user is a store user. Nothing runs beside the store.
//!
//! This crate holds the protocol in two halves. The codec — primitives, the
//! request and response headers, record batches, each API's messages — is
//! pure and synchronous. The surface that serves it runs on the node's tokio
//! runtime and turns every request into statements on the caller's session.

#![forbid(unsafe_code)]
#![cfg_attr(
    test,
    allow(clippy::expect_used, clippy::unwrap_used, clippy::as_conversions)
)]

pub mod api;
pub mod codec;
mod error;
pub mod header;
pub mod records;
#[cfg(feature = "surface")]
mod surface;

#[cfg(feature = "surface")]
pub use surface::{Database, Node};

pub use error::{Error, Result};
