//! Reading a request's body, and how much of one this surface will read.
//!
//! Every body is read before any credential is checked — the credential decides
//! what the body may do, so the body has to be in hand first. That makes the
//! read itself the one cost an anonymous caller controls, and a read with no
//! ceiling hands them the node's memory: the door bounds how many requests run
//! at once, not how large one is.

use std::io::Read;

use tessari_constants::HTTP_MAX_BODY_BYTES;
use tiny_http::Request;

use crate::respond::Answer;

/// The body as bytes, or the answer that refuses it.
///
/// A declared length past the ceiling is refused before a byte is read. A body
/// that declares none — or declares less and sends more — is read one byte past
/// the ceiling and no further, which is enough to know it is too long.
pub(crate) fn bytes(request: &mut Request) -> Result<Vec<u8>, Answer> {
    read(request, "the request body could not be read")
}

/// The body as text, or the answer that refuses it.
pub(crate) fn text(request: &mut Request) -> Result<String, Answer> {
    let unreadable = "the request body is not text";
    String::from_utf8(read(request, unreadable)?).map_err(|_| Answer::bad_request(unreadable))
}

fn read(request: &mut Request, unreadable: &str) -> Result<Vec<u8>, Answer> {
    if request
        .body_length()
        .is_some_and(|declared| declared > HTTP_MAX_BODY_BYTES)
    {
        return Err(too_large());
    }
    let past = u64::try_from(HTTP_MAX_BODY_BYTES)
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    let mut body = Vec::new();
    request
        .as_reader()
        .take(past)
        .read_to_end(&mut body)
        .map_err(|_| Answer::bad_request(unreadable))?;
    if body.len() > HTTP_MAX_BODY_BYTES {
        return Err(too_large());
    }
    Ok(body)
}

fn too_large() -> Answer {
    Answer::new(
        413,
        format!(r#"{{"error":"the request body is larger than {HTTP_MAX_BODY_BYTES} bytes"}}"#),
    )
}
