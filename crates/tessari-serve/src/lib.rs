//! Stopping a serving process, in the order the stages have to happen.
//!
//! # Why this is a crate and not a module in one of the serving crates
//!
//! Both surfaces need the same four things — a way to say whether they are still
//! willing to take work, a way to be told to stop accepting, a count of what is
//! still in flight, and a way to say when that count has reached zero — and they
//! need them to mean the *same* thing, because
//! a shutdown that drains one surface and abandons the other is not a staged
//! shutdown. Putting this in the wire crate would make the HTTP crate depend on
//! it for no other reason; a copy in each gives two mechanisms that must agree
//! and will eventually not.
//!
//! Neither surface sequences the stages. That belongs to whatever holds both of
//! them, which is the binary — so this crate offers the pieces and the order
//! lives with the process (ADR-0015).
//!
//! # Requests and feeds are counted apart, and that is the whole design
//!
//! ADR-0015's stage 2 waits for in-flight work to finish and stage 3 then ends
//! subscriptions. A single count cannot tell those apart, and the difference is
//! exactly what makes them separate stages: **a request finishes on its own and
//! a subscription never does.** Counted together, stage 2 waits for a feed that
//! will still be there at the deadline, and every shutdown becomes a timeout.
//!
//! So a connection is counted as a request when it arrives and *moves* to the
//! feed count if it becomes a subscription — the same connection, a different
//! promise about whether waiting for it can succeed.
//!
//! # Not ready comes before not listening
//!
//! Refusing connections and being unwilling to serve are also two states rather
//! than one, for a reason with the same shape. A readiness route exists so a
//! load balancer can stop sending work *before* the port goes; if the same flag
//! did both, the port would close at the instant the answer changed and nothing
//! could ever observe it. So stage 0 sets [`Stopping::leaving`] and the process
//! keeps serving for a window, and stage 1 sets the refusal.

#![forbid(unsafe_code)]
// `expect_used` and `as_conversions` govern production code; a test states its own expectations.
#![cfg_attr(test, allow(clippy::expect_used, clippy::as_conversions))]

mod accepting;
mod admitting;
mod bridge;
mod census;
mod error;
mod listening;
mod stopping;
pub mod tls;

pub use crate::accepting::{ACCEPT_PAUSE, passes};
pub use crate::bridge::{Bridge, Bridged};
pub use crate::listening::listen;

pub use admitting::{Admitted, Admitting};
pub use census::{Census, Presenting};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};
pub use stopping::{Busy, Drained, Stopping};

#[cfg(test)]
mod tests;
