//! An HTTP surface for TessariDB.
//!
//! A route that runs a script, and three that say how the node is: whether it is
//! alive, whether it will take work, and the numbers behind both. Not a REST
//! resource tree over tables — that would be a second query
//! language, expressed in URLs, that can say less than the one this store
//! already has. **The language is the API.**
//!
//! # On the runtime, with the store behind a bridge
//!
//! Requests are served by axum on the process's runtime (ADR-0085): a waiting
//! connection costs a task, not a thread. The routes themselves are unchanged
//! and synchronous — a commit is a compare-and-set against a substrate — so each
//! request's route runs on the blocking pool through the node's bridge, which
//! refuses rather than queues when every slot is taken.
//!
//! # How a request says who it is
//!
//! `Authorization: Basic`, and nothing else. A store with no users declared is
//! **open** and runs anything, which is what keeps an empty one usable; the
//! first `DEFINE USER` closes it, and from then on a request without a
//! credential is answered `401`.
//!
//! Basic over plaintext is plaintext: the password is in a header anything on
//! the path can read. This is a credential for a connection an operator already
//! protects — a loopback bind, or a reverse proxy terminating TLS — and the
//! README says so rather than leaving it to be discovered.

#![forbid(unsafe_code)]
// `expect_used` and `as_conversions` govern production code; a test states its own expectations.
#![cfg_attr(test, allow(clippy::expect_used, clippy::as_conversions))]

mod basic;
mod body;
#[cfg(feature = "console")]
mod console;
mod dispatch;
#[cfg(not(feature = "console"))]
mod console {
    use axum::http::Method;

    use crate::respond::Answer;

    pub(crate) const fn asset(
        _method: &Method,
        _path: &str,
        _if_none_match: Option<&str>,
    ) -> Option<Answer> {
        None
    }
}
mod incoming;
mod json;
mod kv;
mod listening;
mod node;
mod object;
mod protect;
mod request;
mod respond;
mod securing;
mod snapshot;
mod tokens;
mod websocket;

use std::net::{SocketAddr, TcpListener};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::serve::ListenerExt;
use tessari_constants::{MAX_CONNECTIONS, MAX_STORE_CALLS};
use tessari_serve::{Admitting, Bridge, Bridged, Census, Stopping};
use tessaridb::Db;
use tessaridb::feed::Commits;
use tokio_util::sync::CancellationToken;

use crate::incoming::Incoming;
pub(crate) use dispatch::Shared;
use dispatch::{handle, to_response};
pub use node::{Node, WireDoor, WireSession};
pub use respond::scripts::render_coordinated;
pub use respond::{Answer, refusal_kind};
