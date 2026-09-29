//! The opening handshake's refusals — RFC 6455 §4.2.
//!
//! The upgrade itself, the accept key and the frames are axum's. What stays here
//! is what this node says to a request that is not a websocket upgrade it can
//! answer: those answers are part of this surface's contract, and a framework's
//! own rejection text would change them. They are checked before axum is asked
//! to upgrade, so a request that reaches it is one it accepts.
//!
//! # Every extension is declined, and declining is not the same as ignoring
//!
//! A browser offers `permessage-deflate` on every connection it opens. An
//! extension is accepted by naming it in the response and declined by leaving
//! the header out; axum is built here without compression, so the response
//! names none — the decline.

use axum::http::HeaderMap;

/// The version of the protocol this node speaks, the only one there is.
const VERSION: &str = "13";

/// Why an upgrade was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// Not an upgrade at all — somebody asked this route for a page.
    NotAnUpgrade,
    /// An upgrade to a version this node does not speak.
    Version,
    /// An upgrade with no key to prove understanding with.
    NoKey,
}

impl Refusal {
    /// The status and the body the refusal is answered with.
    pub(crate) const fn answer(self) -> (u16, &'static str) {
        match self {
            Self::NotAnUpgrade => (
                426,
                r#"{"error":"this route serves a websocket; upgrade, or run scripts at POST /script"}"#,
            ),
            Self::Version => (426, r#"{"error":"this node speaks websocket version 13"}"#),
            Self::NoKey => (
                400,
                r#"{"error":"the upgrade carried no Sec-WebSocket-Key"}"#,
            ),
        }
    }
}

/// Whether `headers` ask for an upgrade this node can answer.
pub(crate) fn check(headers: &HeaderMap) -> Result<(), Refusal> {
    if !names(headers, "Upgrade", "websocket") || !names(headers, "Connection", "upgrade") {
        return Err(Refusal::NotAnUpgrade);
    }
    if !names(headers, "Sec-WebSocket-Version", VERSION) {
        return Err(Refusal::Version);
    }
    match value(headers, "Sec-WebSocket-Key") {
        Some(key) if !key.is_empty() => Ok(()),
        _ => Err(Refusal::NoKey),
    }
}

fn value<'a>(headers: &'a HeaderMap, field: &'static str) -> Option<&'a str> {
    headers.get(field).and_then(|value| value.to_str().ok())
}

/// Whether a comma-separated header carries `token`, without regard to case —
/// `Connection: keep-alive, Upgrade` is what a browser sends.
fn names(headers: &HeaderMap, field: &'static str, token: &str) -> bool {
    value(headers, field).is_some_and(|found| {
        found
            .split(',')
            .any(|part| part.trim().eq_ignore_ascii_case(token))
    })
}

#[cfg(test)]
mod tests {
    use axum::http::{HeaderMap, HeaderName, HeaderValue};

    use super::{Refusal, check};

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        pairs
            .iter()
            .filter_map(|(field, value)| {
                Some((
                    HeaderName::from_bytes(field.as_bytes()).ok()?,
                    HeaderValue::from_str(value).ok()?,
                ))
            })
            .collect()
    }

    fn browser() -> Vec<(&'static str, &'static str)> {
        vec![
            ("Host", "localhost"),
            ("Upgrade", "websocket"),
            ("Connection", "keep-alive, Upgrade"),
            ("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ=="),
            ("Sec-WebSocket-Version", "13"),
            ("Sec-WebSocket-Extensions", "permessage-deflate"),
        ]
    }

    #[test]
    fn a_browsers_request_is_upgraded() {
        assert_eq!(check(&headers(&browser())), Ok(()));
    }

    #[test]
    fn the_upgrade_tokens_are_read_out_of_a_list_and_without_case() {
        let mut pairs = browser();
        pairs[1] = ("Upgrade", "WebSocket");
        pairs[2] = ("Connection", "Keep-Alive, UPGRADE");
        assert!(check(&headers(&pairs)).is_ok());
    }

    #[test]
    fn an_ordinary_request_is_told_what_the_route_speaks() {
        assert_eq!(
            check(&headers(&[("Host", "localhost")])),
            Err(Refusal::NotAnUpgrade)
        );
        assert_eq!(Refusal::NotAnUpgrade.answer().0, 426);
    }

    #[test]
    fn another_version_is_refused_by_naming_ours() {
        let mut pairs = browser();
        pairs[4] = ("Sec-WebSocket-Version", "8");
        assert_eq!(check(&headers(&pairs)), Err(Refusal::Version));
        let (status, body) = Refusal::Version.answer();
        assert_eq!(status, 426);
        assert!(body.contains("13"), "the refusal must name the version");
    }

    #[test]
    fn an_upgrade_without_a_key_cannot_be_answered() {
        let mut pairs = browser();
        pairs.remove(3);
        assert_eq!(check(&headers(&pairs)), Err(Refusal::NoKey));
        assert_eq!(Refusal::NoKey.answer().0, 400);
    }
}
