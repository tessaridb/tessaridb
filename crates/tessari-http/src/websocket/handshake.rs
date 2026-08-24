//! The opening handshake — RFC 6455 §4.2.
//!
//! A client offers, and a server that understood proves it by hashing the
//! client's key with a fixed GUID and returning the result. The proof is the
//! only part a client checks, so it is the only part worth testing by value
//! rather than by status code: a server answering `101` and nothing else passes
//! a test that reads the status.
//!
//! # Every extension is declined, and declining is not the same as ignoring
//!
//! A browser offers `permessage-deflate` on every connection it opens. An
//! extension is accepted by naming it in the response and declined by leaving
//! the header out — there is no third state, so a server that neither
//! implements nor answers has already agreed to something it cannot do. The
//! response here carries no `Sec-WebSocket-Extensions` at all, which is the
//! decline, and the test asserts its absence rather than its content.

use tiny_http::Header;

use super::sha1;

/// The GUID RFC 6455 §1.3 fixes, quoted from the specification.
const GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// The one version of the protocol this node speaks.
const VERSION: &str = "13";

/// Standard base64's alphabet.
const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Why a request that reached the socket route is not one this node can upgrade.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// An ordinary request. It reached a route that only speaks one protocol.
    NotAnUpgrade,
    /// An upgrade to a version of the protocol this node does not speak.
    Version,
    /// An upgrade with no key, which there is no way to answer.
    NoKey,
}

impl Refusal {
    /// What to tell a client that asked this way.
    ///
    /// `426` for both protocol faults, because the fault is the same one seen
    /// from two distances: the request cannot be served as it stands and would
    /// be served after an upgrade this node names.
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

/// The `Sec-WebSocket-Accept` value proving this node read `client_key`.
pub(crate) fn accept_key(client_key: &str) -> String {
    let mut joined = String::from(client_key);
    joined.push_str(GUID);
    encode(sha1::digest(joined.as_bytes()))
}

/// Read an upgrade request, answering with the accept value it is owed.
///
/// # Errors
///
/// Returns why the request cannot be upgraded.
pub(crate) fn read(headers: &[Header]) -> Result<String, Refusal> {
    // Both headers are token lists rather than single values — a browser sends
    // `Connection: keep-alive, Upgrade` — so a comparison against the whole
    // value would fail on a request that is perfectly correct.
    if !names(headers, "Upgrade", "websocket") || !names(headers, "Connection", "upgrade") {
        return Err(Refusal::NotAnUpgrade);
    }
    if !names(headers, "Sec-WebSocket-Version", VERSION) {
        return Err(Refusal::Version);
    }
    let key = value(headers, "Sec-WebSocket-Key").ok_or(Refusal::NoKey)?;
    if key.is_empty() {
        return Err(Refusal::NoKey);
    }
    Ok(accept_key(key))
}

/// The value of the first header called `field`.
fn value<'a>(headers: &'a [Header], field: &'static str) -> Option<&'a str> {
    headers
        .iter()
        .find(|header| header.field.equiv(field))
        .map(|header| header.value.as_str())
}

/// Whether `field` is a comma-separated list containing `token`.
fn names(headers: &[Header], field: &'static str, token: &str) -> bool {
    value(headers, field).is_some_and(|found| {
        found
            .split(',')
            .any(|part| part.trim().eq_ignore_ascii_case(token))
    })
}

/// Standard base64 over the twenty bytes of a digest.
///
/// Not a general encoder: the length is fixed, so the padding has one shape.
/// The strict *decoder* next door in `basic.rs` is a different job with
/// different rules, and merging them would be a refactor this needed no part of.
fn encode(digest: [u8; 20]) -> String {
    let mut out = String::with_capacity(28);
    for group in digest.chunks(3) {
        let held = match group {
            [first, second, third] => {
                (u32::from(*first) << 16) | (u32::from(*second) << 8) | u32::from(*third)
            }
            [first, second] => (u32::from(*first) << 16) | (u32::from(*second) << 8),
            [first] => u32::from(*first) << 16,
            // `chunks(3)` never yields an empty group.
            _ => continue,
        };
        // A group of three bytes is four characters, of two is three, of one is
        // two — and the rest is padding.
        let carried = group.len().saturating_add(1);
        for slot in 0..carried {
            let shift = 18_usize.saturating_sub(slot.saturating_mul(6));
            let sextet = usize::try_from((held >> shift) & 0b11_1111).unwrap_or(0);
            out.push(char::from(ALPHABET[sextet]));
        }
        for _ in carried..4 {
            out.push('=');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use tiny_http::Header;

    use super::{Refusal, accept_key, read};

    fn headers(pairs: &[(&str, &str)]) -> Vec<Header> {
        pairs
            .iter()
            .filter_map(|(field, value)| {
                Header::from_bytes(field.as_bytes(), value.as_bytes()).ok()
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
    fn the_published_example_is_reproduced() {
        // RFC 6455 §1.3, key and answer both quoted from the specification. This
        // is the whole handshake: a client checks this value and nothing else.
        assert_eq!(
            accept_key("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=",
            "the accept value does not match the one the specification prints"
        );
    }

    #[test]
    fn a_browsers_request_is_upgraded() {
        assert_eq!(
            read(&headers(&browser())),
            Ok("s3pPLMBiTxaQ9kYGzzhZRbK+xOo=".to_owned()),
            "a request shaped exactly like a browser's was refused"
        );
    }

    #[test]
    fn the_upgrade_tokens_are_read_out_of_a_list_and_without_case() {
        let mut pairs = browser();
        pairs[1] = ("Upgrade", "WebSocket");
        pairs[2] = ("Connection", "Keep-Alive, upgrade");
        assert!(
            read(&headers(&pairs)).is_ok(),
            "the tokens are case-insensitive and `Connection` is a list"
        );
    }

    #[test]
    fn an_ordinary_request_is_told_what_the_route_speaks() {
        assert_eq!(
            read(&headers(&[("Host", "localhost")])),
            Err(Refusal::NotAnUpgrade),
        );
        assert_eq!(Refusal::NotAnUpgrade.answer().0, 426);
    }

    #[test]
    fn another_version_is_refused_by_naming_ours() {
        let mut pairs = browser();
        pairs[4] = ("Sec-WebSocket-Version", "8");
        assert_eq!(read(&headers(&pairs)), Err(Refusal::Version));
        let (status, body) = Refusal::Version.answer();
        assert_eq!(status, 426);
        assert!(
            body.contains("13"),
            "a version refusal that does not say which version is one nobody can act on"
        );
    }

    #[test]
    fn an_upgrade_without_a_key_cannot_be_answered() {
        let mut pairs = browser();
        pairs[3] = ("Sec-WebSocket-Key", "");
        assert_eq!(read(&headers(&pairs)), Err(Refusal::NoKey));
        assert_eq!(Refusal::NoKey.answer().0, 400);
    }

    #[test]
    fn every_key_length_encodes_to_twenty_eight_characters() {
        // The digest is always twenty bytes, so the answer is always twenty-eight
        // characters ending in one `=`. A padding mistake shows here rather than
        // in a client's error console.
        for key in ["", "a", "dGhlIHNhbXBsZSBub25jZQ==", &"x".repeat(500)] {
            let answer = accept_key(key);
            assert_eq!(answer.len(), 28, "key of {} bytes", key.len());
            assert!(answer.ends_with('='), "key of {} bytes", key.len());
            assert_eq!(
                answer.matches('=').count(),
                1,
                "twenty bytes take exactly one padding character"
            );
        }
    }
}
