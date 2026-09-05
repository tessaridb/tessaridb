//! The wire protocol: framed TCP carrying values in the store's own encoding.
//!
//! # The decision this crate exists to take
//!
//! The HTTP endpoint chose a synchronous server and named where the real
//! decision belonged: *"a thread per concurrent request is right for one node
//! and wrong for ten thousand idle connections, and when that is the problem it
//! belongs to the wire protocol taking the async decision deliberately."*
//!
//! Taken here, and the answer is **synchronous, a thread per connection, no
//! runtime**:
//!
//! - The store below is synchronous by design, because a commit is a
//!   compare-and-set against a substrate. An async server over it would put
//!   `spawn_blocking` at every call, which is a thread pool wearing a runtime's
//!   clothes and a large dependency to wear them.
//! - The cost is a thread per **connection**, and a subscription holds one open —
//!   so the ceiling is subscribers rather than requests. Hundreds is fine. Tens
//!   of thousands is not, and that is the trigger for revisiting this: idle
//!   subscribers outnumbering what a thread each is worth.
//! - It needs **no dependency at all**. `std::net`, length-prefixed frames, and
//!   the encoding crate. For a network-facing surface that is worth more than
//!   the convenience of a framework.
//!
//! # Why not JSON, when there is already an endpoint that speaks it
//!
//! Because JSON has six types and this store has fifteen. The HTTP surface pays
//! that price deliberately — a browser is owed JSON — and quotes a decimal so it
//! is not silently a double. A client reading that back has to *guess*: is
//! `"12.34"` a decimal, and `"2s"` a duration? Here a value goes through the
//! codec the store writes records with, so fifteen types go out and fifteen come
//! back, and neither end decides anything.
//!
//! # What it is not, yet
//!
//! **Subscription push** is W2. The frames are kinded rather than a plain
//! request-and-reply precisely so a server can send something the client did not
//! ask for, and the tags above 3 are reserved for it.
//!
//! **There is no TLS.** A protocol that carries credentials in the clear belongs
//! on a trusted network and nowhere else, and this says so rather than leaving it
//! to be assumed.

#![forbid(unsafe_code)]

mod client;
mod error;
mod frame;
mod message;
#[cfg(feature = "server")]
mod node;
mod push;

#[cfg(feature = "server")]
use std::time::Duration;

pub use crate::client::{Client, Feed};
pub use crate::error::{Error, Result};
#[cfg(feature = "server")]
pub use crate::message::names_for;
pub use crate::message::{Answer, Correction, Exact, Names, Remark, Request, Suggested, spell};
#[cfg(feature = "server")]
pub use crate::node::Node;
pub use crate::push::{Became, Follow, Happened};

/// How long the node will wait for a subscriber to accept a change.
///
/// Server-side only, and gated with the rest of it.
///
/// When a client stops reading, its socket fills and this write blocks. Rather
/// than hold a thread forever the connection ends — see `push.rs` for why
/// nothing is lost by that, and why buffering here instead would rebuild the
/// queue the feed design removed.
#[cfg(feature = "server")]
const READING: Duration = Duration::from_secs(30);
