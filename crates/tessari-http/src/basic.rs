//! Reading `Authorization: Basic` — who a request says it is.
//!
//! # Why a credential arrives in a header and not in the script
//!
//! A script is text a caller composes, logs, pastes into an issue and sends
//! through a proxy. A password in one is a password in all of those, which is
//! why `SIGNIN` is not a statement. The header is where a credential belongs,
//! and it is the one place a proxy is already expected to redact.
//!
//! # Why the base64 is written here
//!
//! It is twenty lines with a fixed alphabet and a defined padding rule, and it
//! sits on the authentication path — which is the last place to add a dependency
//! whose behaviour nobody in this repository has read. The same argument the
//! JSON encoder was written under, with more at stake.
//!
//! Decoding is strict: a character outside the alphabet, a length that is not a
//! multiple of four, or padding in the middle is **rejected rather than
//! repaired**. A lenient decoder turns one malformed header into two different
//! credentials depending on which end reads it.
//!
//! # What this is not
//!
//! Basic over plaintext is plaintext. This carries a password in a header that
//! anything on the path can read, so it is a credential for a connection an
//! operator already protects — a loopback bind, or a reverse proxy terminating
//! TLS. That is stated in the README rather than implied here.

/// The name and password a request presents, if it presents any.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Credentials {
    /// The user name.
    pub(crate) name: String,
    /// The password, in the clear, for as long as it takes to verify it.
    pub(crate) password: String,
}

/// Read credentials out of an `Authorization` header value.
///
/// Returns `None` for anything that is not a well-formed `Basic` credential:
/// another scheme, a payload that is not base64, or one carrying no colon. A
/// caller treats that the same as no header at all, because a header this node
/// cannot read is a header it does not know the sender by.
pub(crate) fn read(header: &str) -> Option<Credentials> {
    let payload = header
        .strip_prefix("Basic ")
        .or_else(|| header.strip_prefix("basic "))?;
    let decoded = decode(payload.trim())?;
    let text = String::from_utf8(decoded).ok()?;
    // The first colon separates them, because a name may not contain one and a
    // password very much may.
    let (name, password) = text.split_once(':')?;
    Some(Credentials {
        name: name.to_owned(),
        password: password.to_owned(),
    })
}

/// The sextet a base64 character stands for.
const fn sextet(character: u8) -> Option<u8> {
    match character {
        // The range on each arm is what makes the subtraction sound; the
        // saturating form says so to the compiler as well as to a reader.
        b'A'..=b'Z' => Some(character.saturating_sub(b'A')),
        b'a'..=b'z' => Some(character.saturating_sub(b'a').saturating_add(26)),
        b'0'..=b'9' => Some(character.saturating_sub(b'0').saturating_add(52)),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

/// Decode standard base64, strictly.
fn decode(text: &str) -> Option<Vec<u8>> {
    let bytes = text.as_bytes();
    if bytes.is_empty() || bytes.len() % 4 != 0 {
        return None;
    }
    let quads = bytes.len() / 4;
    let mut out = Vec::with_capacity(quads.saturating_mul(3));
    for (index, quad) in bytes.chunks_exact(4).enumerate() {
        // Padding is only ever the last one or two characters of the last group.
        // Anywhere else it is a malformed document rather than a short one, and
        // the two must not be confused.
        let last = index.saturating_add(1) == quads;
        let padding = match (quad[2], quad[3]) {
            (b'=', b'=') => 2,
            (_, b'=') => 1,
            _ => 0,
        };
        if padding > 0 && !last {
            return None;
        }
        let carried = 4_usize.saturating_sub(padding);
        let mut held: u32 = 0;
        for character in &quad[..carried] {
            // An `=` inside the part that is not padding — `YW=j` — is padding
            // in the middle of a group, which `sextet` refuses along with
            // everything else outside the alphabet.
            held = (held << 6) | u32::from(sextet(*character)?);
        }
        // The characters standing in for padding contribute zero bits.
        held <<= 6_u32.saturating_mul(u32::try_from(padding).ok()?);
        let triple = held.to_be_bytes();
        // A quad carries three bytes, less one for each padding character.
        out.extend_from_slice(&triple[1..carried]);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic)]

    use super::{decode, read};

    #[test]
    fn a_well_formed_header_yields_the_name_and_password() {
        // "ada:correct horse"
        let credentials = read("Basic YWRhOmNvcnJlY3QgaG9yc2U=").expect("credentials");
        assert_eq!(credentials.name, "ada");
        assert_eq!(credentials.password, "correct horse");
    }

    #[test]
    fn a_password_may_contain_the_separator_and_a_name_may_not() {
        // "ada:a:b" — the first colon separates, so the password keeps its own.
        let credentials = read("Basic YWRhOmE6Yg==").expect("credentials");
        assert_eq!(credentials.name, "ada");
        assert_eq!(credentials.password, "a:b");
    }

    #[test]
    fn what_this_node_cannot_read_is_no_credential_at_all() {
        // Another scheme, a payload that is not base64, one that is not text,
        // and one carrying no colon are all "I do not know who you are".
        assert!(read("Bearer YWRhOng=").is_none());
        assert!(read("Basic not base64!").is_none());
        assert!(read("Basic YWRh").is_none(), "no colon");
        assert!(read("").is_none());
    }

    #[test]
    fn decoding_is_strict_rather_than_forgiving() {
        // A lenient decoder turns one malformed header into two different
        // credentials depending on which end reads it.
        assert!(decode("YWRh=").is_none(), "length not a multiple of four");
        assert!(decode("YW==YWRh").is_none(), "padding in the middle");
        assert!(decode("YW-h").is_none(), "outside the alphabet");
        assert!(decode("").is_none());
    }

    #[test]
    fn every_padding_length_decodes_to_what_it_encoded() {
        assert_eq!(decode("YQ==").expect("a byte"), b"a");
        assert_eq!(decode("YWI=").expect("two bytes"), b"ab");
        assert_eq!(decode("YWJj").expect("three bytes"), b"abc");
        assert_eq!(decode("YWJjZA==").expect("four bytes"), b"abcd");
    }
}
