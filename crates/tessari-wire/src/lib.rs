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
//! Because JSON has six types and this store has seventeen. The HTTP surface pays
//! that price deliberately — a browser is owed JSON — and quotes a decimal so it
//! is not silently a double. A client reading that back has to *guess*: is
//! `"12.34"` a decimal, and `"2s"` a duration? Here a value goes through the
//! codec the store writes records with, so seventeen types go out and seventeen
//! come back, and neither end decides anything.
//!
//! # Encryption is on one of the two surfaces, and which one matters
//!
//! This crate carries **two** conversations and they are not protected alike.
//!
//! - The **peer link** (`link.rs`) is mutually authenticated TLS. Both ends
//!   present a certificate issued by the cluster authority, each verifies the
//!   other against it, and a peer's name is derived from its node id — so an
//!   unidentified caller cannot reach the door at all.
//! - The **serving surface** — the client-facing wire in `node.rs` — has **no
//!   TLS**, and it is the one that carries a user's password. A protocol that
//!   carries credentials in the clear belongs on a trusted network and nowhere
//!   else, and this says so rather than leaving it to be assumed.
//!
//! The asymmetry is deliberate today and it is not settled: the peer link earns
//! its certificates from a cluster that has an authority to issue them, and the
//! serving surface has no equivalent. What follows from it — a replication
//! stream carries every record of every tenant inside its reach — is an open
//! question recorded outside this repository, not an oversight to be fixed by
//! whoever reads this next.

#![forbid(unsafe_code)]

#[cfg(feature = "server")]
mod campaign;
mod client;
#[cfg(feature = "server")]
mod collection;
#[cfg(feature = "server")]
mod credential;
#[cfg(feature = "server")]
mod directory;
#[cfg(feature = "server")]
mod driver;
mod error;
mod frame;
#[cfg(feature = "server")]
mod grant;
#[cfg(feature = "server")]
mod joining;
#[cfg(feature = "server")]
mod link;
mod message;
#[cfg(feature = "server")]
mod node;
#[cfg(feature = "server")]
mod peer;
mod push;
mod redirect;

#[cfg(feature = "server")]
use std::time::Duration;

#[cfg(feature = "server")]
pub use crate::campaign::{Standing, Stood};
pub use crate::client::{Client, Feed, Served};
#[cfg(feature = "server")]
pub use crate::collection::{Collect, Collected, Collector, NoLog, Origin, Serving, Subscriptions};
#[cfg(feature = "server")]
pub use crate::credential::{fingerprint, names, presented};
#[cfg(feature = "server")]
pub use crate::directory::{Destination, Directory, Heard};
#[cfg(feature = "server")]
pub use crate::driver::{
    Collecting, Published, Renewing, bootstrap_from, due_in, every, heard_a_leader, names_a_peer,
    stands, upstream, voters,
};
pub use crate::error::{Error, Result};
#[cfg(feature = "server")]
pub use crate::grant::{Ballot, Deciding, Leadership, Reached, Refused, Round, Vote, Voter};
#[cfg(feature = "server")]
pub use crate::joining::{Joining, Seed, Told};
#[cfg(feature = "server")]
pub use crate::link::{Answered, Ask, Credential, Met, Peers, call};
#[cfg(feature = "server")]
pub use crate::message::names_for;
pub use crate::message::{Answer, Correction, Exact, Names, Remark, Request, Suggested, spell};
#[cfg(feature = "server")]
pub use crate::node::Node;
#[cfg(feature = "server")]
pub use crate::peer::{Hello, PeerFrame, Presented, Purpose, admit};
pub use crate::push::{Became, Follow, Happened};

pub use crate::redirect::{Elsewhere, Settlement};
/// The authority a cluster is issued by, re-exported because [`Joining`] hands
/// one out and a caller cannot otherwise name the type it holds.
#[cfg(feature = "server")]
pub use rustls::pki_types::CertificateDer;

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
