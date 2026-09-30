//! A request as the routes see it: owned, with its body already in hand.
//!
//! The routes are synchronous and run on the blocking pool, so what they are
//! given cannot borrow a connection. The body is read before they run — by the
//! async side, under the same ceiling and with the same refusals `body.rs`
//! states — and only for the routes that take one.

use axum::body::Body;
use axum::http::{HeaderMap, Method};
use futures_util::StreamExt;
use tessari_constants::HTTP_MAX_BODY_BYTES;

use crate::object;

/// Why a body is not in hand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Unread {
    /// It is, or declared that it would be, larger than the ceiling.
    TooLarge,
    /// The connection failed while it was being read.
    Unreadable,
}

/// One request, owned.
pub(crate) struct Incoming {
    pub(crate) method: Method,
    /// The path with its query, as the request line carried it.
    pub(crate) url: String,
    pub(crate) headers: HeaderMap,
    /// `None` for a route that takes no body, which is therefore never read.
    pub(crate) body: Option<Result<Vec<u8>, Unread>>,
}

impl Incoming {
    /// One header's value as text, if it was sent and is text.
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|value| value.to_str().ok())
    }
}

/// Whether the route `method` and `url` reach reads a body.
///
/// The same routing `answer` does, asked early: a body on a route that ignores
/// one is never read, exactly as before, so a large body on `GET /health` is
/// still answered as health rather than refused as too large.
pub(crate) fn takes_body(method: &Method, url: &str) -> bool {
    match (method, url) {
        (&Method::POST, "/script" | "/password" | "/vault/unseal" | "/vault/passphrase") => true,
        (&Method::POST, url) if url.starts_with("/series/") => true,
        // One vault's own surface (ADR-0093 D6): the two acts that carry a body.
        (method, url) if url.starts_with("/vault/") => {
            let path = url.split('?').next().unwrap_or(url);
            *method == Method::POST && (path.ends_with("/unseal") || path.ends_with("/passphrase"))
        }
        (&Method::PUT | &Method::POST, url) if url.starts_with("/kv/") => true,
        (
            _,
            "/script" | "/session" | "/password" | "/health" | "/ready" | "/metrics" | "/watch"
            | "/vault" | "/vault/seal" | "/vault/unseal" | "/vault/passphrase",
        ) => false,
        (&Method::GET, url) if url == "/backup" || url.starts_with("/backup?") => false,
        (method, url) => {
            matches!(*method, Method::PUT | Method::POST) && object::target(url).is_some()
        }
    }
}

/// Read a body, refusing one past the ceiling without reading further.
///
/// A declared length past the ceiling is refused before a byte is read. A body
/// that declares none — or declares less and sends more — is read one chunk past
/// the ceiling and no further, which is enough to know it is too long.
pub(crate) async fn read(headers: &HeaderMap, body: Body) -> Result<Vec<u8>, Unread> {
    let declared = headers
        .get(axum::http::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    let ceiling = u64::try_from(HTTP_MAX_BODY_BYTES).unwrap_or(u64::MAX);
    if declared.is_some_and(|length| length > ceiling) {
        return Err(Unread::TooLarge);
    }
    let mut held = Vec::new();
    let mut chunks = body.into_data_stream();
    while let Some(chunk) = chunks.next().await {
        let chunk = chunk.map_err(|_| Unread::Unreadable)?;
        held.extend_from_slice(&chunk);
        if held.len() > HTTP_MAX_BODY_BYTES {
            return Err(Unread::TooLarge);
        }
    }
    Ok(held)
}
