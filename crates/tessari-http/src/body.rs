//! Reading a request's body, and how much of one this surface will read.
//!
//! Every body is read before any credential is checked — the credential decides
//! what the body may do, so the body has to be in hand first. That makes the
//! read itself the one cost an anonymous caller controls, and a read with no
//! ceiling hands them the node's memory: the door bounds how many requests run
//! at once, not how large one is.

use tessari_constants::HTTP_MAX_BODY_BYTES;

use crate::incoming::{Incoming, Unread};
use crate::respond::Answer;

/// The body as bytes, or the answer that refuses it.
///
/// Read before the route ran, under the ceiling — see `incoming::read`.
pub(crate) fn bytes(request: &mut Incoming) -> Result<Vec<u8>, Answer> {
    read(request, "the request body could not be read")
}

/// The body as text, or the answer that refuses it.
pub(crate) fn text(request: &mut Incoming) -> Result<String, Answer> {
    let unreadable = "the request body is not text";
    String::from_utf8(read(request, unreadable)?).map_err(|_| Answer::bad_request(unreadable))
}

fn read(request: &mut Incoming, unreadable: &str) -> Result<Vec<u8>, Answer> {
    match request.body.take() {
        Some(Ok(body)) => Ok(body),
        Some(Err(Unread::TooLarge)) => Err(too_large()),
        // A route that takes a body is always given one read; `None` here would
        // be a route `incoming::takes_body` does not know about, answered as an
        // empty body would have been.
        Some(Err(Unread::Unreadable)) => Err(Answer::bad_request(unreadable)),
        None => Ok(Vec::new()),
    }
}

fn too_large() -> Answer {
    Answer::new(
        413,
        format!(r#"{{"error":"the request body is larger than {HTTP_MAX_BODY_BYTES} bytes"}}"#),
    )
}
