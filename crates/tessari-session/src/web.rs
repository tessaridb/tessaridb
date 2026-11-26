//! URLs and IP addresses as readings over strings (G102 C7 pack 2).
//!
//! A URL is parsed by a hand-rolled RFC 3986 subset — `scheme://authority
//! path ?query #fragment`, with the authority's `user@` and `:port` — and an
//! address by `std::net`'s own parsers, which are exact and are not a new
//! dependency. The accessors answer `NONE` where the URL names no such part,
//! by the rule every domain-end answer in the language follows; a text that
//! is not a URL at all answers `NONE` from every accessor rather than
//! refusing, because reading a field of a million rows must not stop at the
//! one row somebody typed wrong. `ip::in_range` is the one semantic function
//! and the one that refuses: a range test against an address nobody can
//! parse has no answer, only a mistake.

use tessari_ql::Function;
use tessari_types::Value;

use tessari_ql::Span;

use crate::call::wrong_type;
use crate::error::Result;

/// One URL, split where RFC 3986 splits it.
struct Parts<'a> {
    scheme: String,
    host: Option<&'a str>,
    port: Option<u16>,
    path: &'a str,
    query: Option<&'a str>,
    fragment: Option<&'a str>,
}

/// The URL `text` spells, or `None` when it spells none.
///
/// The subset is the one the brief fixes: a scheme, then `//` and an
/// authority — `[user@]host[:port]`, a bracketed IPv6 literal allowed — or no
/// authority at all, and then a path, a query and a fragment in that order.
/// A port that is not digits, or a scheme that is not one, makes the whole
/// text not a URL.
fn parts(text: &str) -> Option<Parts<'_>> {
    let (scheme, rest) = text.split_once(':')?;
    let mut letters = scheme.chars();
    let first = letters.next()?;
    if !first.is_ascii_alphabetic()
        || !letters.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    {
        return None;
    }
    let scheme = scheme.to_ascii_lowercase();
    let (mut host, mut port) = (None, None);
    let rest = match rest.strip_prefix("//") {
        Some(after) => {
            let end = after.find(['/', '?', '#']).unwrap_or(after.len());
            let authority = after.get(..end)?;
            let rest = after.get(end..)?;
            // `user@` is dropped, never answered: nothing reads it back.
            let authority = authority
                .rsplit_once('@')
                .map_or(authority, |(_, after_user)| after_user);
            let (named, held_port) = match authority.strip_prefix('[') {
                // A bracketed IPv6 literal: the colon inside is not a port's.
                Some(inside) => {
                    let close = inside.find(']')?;
                    let name = inside.get(..close)?;
                    let after = inside.get(close.saturating_add(1)..)?;
                    (name, after.strip_prefix(':'))
                }
                None => match authority.rsplit_once(':') {
                    Some((name, held)) => (name, Some(held)),
                    None => (authority, None),
                },
            };
            if named.is_empty() {
                return None;
            }
            host = Some(named);
            if let Some(held) = held_port {
                if held.is_empty() || !held.bytes().all(|b| b.is_ascii_digit()) {
                    return None;
                }
                port = held.parse().ok();
            }
            rest
        }
        None => rest,
    };
    let (rest, fragment) = match rest.split_once('#') {
        Some((before, fragment)) => (before, Some(fragment)),
        None => (rest, None),
    };
    let (path, query) = match rest.split_once('?') {
        Some((before, query)) => (before, Some(query)),
        None => (rest, None),
    };
    Some(Parts {
        scheme,
        host,
        port,
        path,
        query,
        fragment,
    })
}

/// One of the six accessors, by the function that was called.
pub(crate) fn url_part(function: Function, text: &str, span: Span) -> Result<Value> {
    let Some(split) = parts(text) else {
        return Ok(Value::None);
    };
    Ok(match function {
        Function::UrlScheme => Value::from(split.scheme),
        Function::UrlHost => split.host.map_or(Value::None, Value::from),
        Function::UrlPort => split.port.map_or(Value::None, |held| {
            Value::Number(tessari_types::Number::Integer(i64::from(held)))
        }),
        Function::UrlPath => Value::from(split.path),
        Function::UrlQuery => split.query.map_or(Value::None, Value::from),
        Function::UrlFragment => split.fragment.map_or(Value::None, Value::from),
        _ => {
            debug_assert!(false, "not a url accessor: {function:?}");
            let _ = span;
            Value::None
        }
    })
}

/// A byte that needs no escaping in a URL: RFC 3986's unreserved set.
fn unreserved(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~')
}

/// The text with every byte outside the unreserved set percent-escaped.
pub(crate) fn encode(text: &str) -> Value {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if unreserved(byte) {
            out.push(char::from(byte));
        } else {
            out.push('%');
            out.push(char::from(HEX[usize::from(byte >> 4)]));
            out.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
    }
    Value::from(out)
}

/// The text with its percent-escapes read; `None` when one is malformed or
/// the bytes it spells are not text.
pub(crate) fn decode(text: &str) -> Value {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0_usize;
    while let Some(byte) = bytes.get(at) {
        if *byte == b'%' {
            let (Some(high), Some(low)) = (
                bytes.get(at.saturating_add(1)).and_then(|b| hex(*b)),
                bytes.get(at.saturating_add(2)).and_then(|b| hex(*b)),
            ) else {
                return Value::None;
            };
            out.push(high << 4 | low);
            at = at.saturating_add(3);
        } else {
            out.push(*byte);
            at = at.saturating_add(1);
        }
    }
    String::from_utf8(out).map_or(Value::None, Value::from)
}

/// One hex digit's value.
fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte.wrapping_sub(b'0')),
        b'a'..=b'f' => Some(byte.wrapping_sub(b'a').wrapping_add(10)),
        b'A'..=b'F' => Some(byte.wrapping_sub(b'A').wrapping_add(10)),
        _ => None,
    }
}

/// An address as either family, for the range test.
enum Address {
    V4(std::net::Ipv4Addr),
    V6(std::net::Ipv6Addr),
}

/// The address `text` spells, either family.
fn address(text: &str) -> Option<Address> {
    if let Ok(v4) = text.parse::<std::net::Ipv4Addr>() {
        return Some(Address::V4(v4));
    }
    text.parse::<std::net::Ipv6Addr>().ok().map(Address::V6)
}

/// Whether `ip` is inside `cidr`, refused when either does not parse.
pub(crate) fn in_range(ip: &str, cidr: &str, span: Span) -> Result<Value> {
    let refused = |at: usize, expected: &'static str| {
        wrong_type(
            Function::IpInRange,
            at,
            expected,
            "text that is not one",
            span,
        )
    };
    let Some(held) = address(ip) else {
        return Err(refused(0, "an IPv4 or IPv6 address"));
    };
    let Some((base, prefix)) = cidr.split_once('/') else {
        return Err(refused(1, "a CIDR range, address/prefix"));
    };
    let Some(base) = address(base) else {
        return Err(refused(1, "a CIDR range, address/prefix"));
    };
    let Ok(prefix) = prefix.parse::<u32>() else {
        return Err(refused(1, "a CIDR range, address/prefix"));
    };
    let inside = match (held, base) {
        (Address::V4(one), Address::V4(base)) if prefix <= 32 => {
            let mask = if prefix == 0 {
                0
            } else {
                u32::MAX << (32_u32.saturating_sub(prefix))
            };
            u32::from(one) & mask == u32::from(base) & mask
        }
        (Address::V6(one), Address::V6(base)) if prefix <= 128 => {
            let mask = if prefix == 0 {
                0
            } else {
                u128::MAX << (128_u32.saturating_sub(prefix))
            };
            u128::from(one) & mask == u128::from(base) & mask
        }
        // One family inside the other's range is not inside it — a truth,
        // not a mistake.
        _ => false,
    };
    Ok(Value::Bool(inside))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic, clippy::unwrap_used)]

    use super::*;

    #[test]
    fn a_url_splits_where_the_standard_splits_it() {
        let held = parts("https://user@example.com:8443/a/b?x=1#top").unwrap();
        assert_eq!(held.scheme, "https");
        assert_eq!(held.host, Some("example.com"));
        assert_eq!(held.port, Some(8443));
        assert_eq!(held.path, "/a/b");
        assert_eq!(held.query, Some("x=1"));
        assert_eq!(held.fragment, Some("top"));
    }

    #[test]
    fn what_is_not_a_url_is_none_everywhere() {
        assert!(parts("not a url").is_none());
        assert!(parts("http://host:notaport/").is_none());
        assert!(parts("1http://x/").is_none());
    }

    #[test]
    fn a_bracketed_v6_host_keeps_its_colons() {
        let held = parts("http://[2001:db8::1]:80/x").unwrap();
        assert_eq!(held.host, Some("2001:db8::1"));
        assert_eq!(held.port, Some(80));
    }

    #[test]
    fn percent_round_trips_and_refuses_a_bad_escape() {
        let Value::String(escaped) = encode("a b/ü") else {
            panic!("encode answered no text");
        };
        assert_eq!(escaped, "a%20b%2F%C3%BC");
        assert_eq!(decode(&escaped), Value::from("a b/ü"));
        assert_eq!(decode("a%2"), Value::None);
        assert_eq!(decode("%zz"), Value::None);
    }

    #[test]
    fn ranges_hold_what_they_should() {
        assert_eq!(
            in_range("10.1.2.3", "10.0.0.0/8", Span::new(0, 0)).unwrap(),
            Value::Bool(true)
        );
        assert_eq!(
            in_range("11.1.2.3", "10.0.0.0/8", Span::new(0, 0)).unwrap(),
            Value::Bool(false)
        );
        assert_eq!(
            in_range("2001:db8::1", "2001:db8::/32", Span::new(0, 0)).unwrap(),
            Value::Bool(true)
        );
        assert_eq!(
            in_range("2001:db8::1", "10.0.0.0/8", Span::new(0, 0)).unwrap(),
            Value::Bool(false)
        );
        assert!(in_range("not-an-ip", "10.0.0.0/8", Span::new(0, 0)).is_err());
        assert!(in_range("10.0.0.1", "10.0.0.0", Span::new(0, 0)).is_err());
    }
}
