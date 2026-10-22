//! Replies written in the protocol version the connection speaks.
//!
//! RESP2 has no map and one null per shape; RESP3 has both. A reply is built
//! once as what it means and written in whichever the client chose with
//! `HELLO`, so no command has to know which that was.

use std::borrow::Cow;

/// The protocol version a connection speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Protocol {
    /// RESP2, which every connection starts in.
    #[default]
    Two,
    /// RESP3, chosen with `HELLO 3`.
    Three,
}

/// One reply, as what it means.
#[derive(Debug, Clone, PartialEq)]
pub enum Reply {
    /// A short status: `OK`, `PONG`, `QUEUED`.
    Status(Cow<'static, str>),
    /// An error, its first word the class a client branches on (`ERR`,
    /// `WRONGTYPE`, `NOAUTH`, …).
    Error(Cow<'static, str>),
    /// A signed integer.
    Integer(i64),
    /// Bytes.
    Bulk(Vec<u8>),
    /// Nothing: no such key, a condition that did not hold.
    Nil,
    /// Nothing where an array was asked for — a blocking pop that timed out, a
    /// multi-key pop that found nothing, a transaction a watched key stopped:
    /// `*-1` in RESP2, where a client tells it from a nil string.
    NilArray,
    /// A sequence of replies.
    Array(Vec<Reply>),
    /// Pairs, written as a map in RESP3 and a flat array in RESP2.
    Map(Vec<(Reply, Reply)>),
    /// A floating-point number as its digits: a double in RESP3, bytes in
    /// RESP2.
    Double(Vec<u8>),
    /// Ordered pairs that may repeat a key — a member and its score — each
    /// an array of two in RESP3, and one flat array in RESP2.
    Pairs(Vec<(Reply, Reply)>),
    /// Something the node says without being asked — a published message,
    /// a subscription's notice: a push in RESP3, an array in RESP2.
    Push(Vec<Reply>),
    /// Several replies to one command, each written as its own — `SUBSCRIBE
    /// a b` answers one notice per channel.
    Several(Vec<Reply>),
}

impl Reply {
    /// `OK`.
    pub const OK: Self = Self::Status(Cow::Borrowed("OK"));

    /// An error reply whose text is `message`, its class word first.
    pub fn error(message: impl Into<Cow<'static, str>>) -> Self {
        Self::Error(message.into())
    }

    /// Bytes of text.
    pub fn text(text: impl Into<Vec<u8>>) -> Self {
        Self::Bulk(text.into())
    }
}

/// Append `reply` to `out` in `protocol`.
pub fn encode(reply: &Reply, protocol: Protocol, out: &mut Vec<u8>) {
    match reply {
        Reply::Status(text) => line(out, b'+', text.as_bytes()),
        Reply::Error(text) => line(out, b'-', text.as_bytes()),
        Reply::Integer(integer) => {
            out.push(b':');
            decimal(out, *integer);
            out.extend_from_slice(b"\r\n");
        }
        Reply::Bulk(bytes) => {
            out.push(b'$');
            decimal(out, length(bytes.len()));
            out.extend_from_slice(b"\r\n");
            out.extend_from_slice(bytes);
            out.extend_from_slice(b"\r\n");
        }
        Reply::Nil => match protocol {
            Protocol::Two => out.extend_from_slice(b"$-1\r\n"),
            Protocol::Three => out.extend_from_slice(b"_\r\n"),
        },
        Reply::NilArray => match protocol {
            Protocol::Two => out.extend_from_slice(b"*-1\r\n"),
            Protocol::Three => out.extend_from_slice(b"_\r\n"),
        },
        Reply::Array(items) => sequence(out, b'*', items, protocol),
        Reply::Push(items) => match protocol {
            Protocol::Two => sequence(out, b'*', items, protocol),
            Protocol::Three => sequence(out, b'>', items, protocol),
        },
        Reply::Several(replies) => {
            for reply in replies {
                encode(reply, protocol, out);
            }
        }
        Reply::Map(pairs) => {
            match protocol {
                Protocol::Two => {
                    out.push(b'*');
                    decimal(out, length(pairs.len().saturating_mul(2)));
                }
                Protocol::Three => {
                    out.push(b'%');
                    decimal(out, length(pairs.len()));
                }
            }
            out.extend_from_slice(b"\r\n");
            for (key, value) in pairs {
                encode(key, protocol, out);
                encode(value, protocol, out);
            }
        }
        Reply::Double(digits) => match protocol {
            Protocol::Two => encode(&Reply::Bulk(digits.clone()), protocol, out),
            Protocol::Three => line(out, b',', digits),
        },
        Reply::Pairs(pairs) => {
            let (count, width) = match protocol {
                Protocol::Two => (pairs.len().saturating_mul(2), None),
                Protocol::Three => (pairs.len(), Some(2)),
            };
            out.push(b'*');
            decimal(out, length(count));
            out.extend_from_slice(b"\r\n");
            for (first, second) in pairs {
                if let Some(width) = width {
                    out.push(b'*');
                    decimal(out, width);
                    out.extend_from_slice(b"\r\n");
                }
                encode(first, protocol, out);
                encode(second, protocol, out);
            }
        }
    }
}

/// Append a push of `parts`, each as bytes, to `out` in `protocol` — what a
/// published message is written as, straight from the bytes every subscriber
/// shares, with no reply built for it.
pub fn encode_push(parts: &[&[u8]], protocol: Protocol, out: &mut Vec<u8>) {
    out.push(match protocol {
        Protocol::Two => b'*',
        Protocol::Three => b'>',
    });
    decimal(out, length(parts.len()));
    out.extend_from_slice(b"\r\n");
    for part in parts {
        out.push(b'$');
        decimal(out, length(part.len()));
        out.extend_from_slice(b"\r\n");
        out.extend_from_slice(part);
        out.extend_from_slice(b"\r\n");
    }
}

/// A count of `items` behind `kind`, then each of them.
fn sequence(out: &mut Vec<u8>, kind: u8, items: &[Reply], protocol: Protocol) {
    out.push(kind);
    decimal(out, length(items.len()));
    out.extend_from_slice(b"\r\n");
    for item in items {
        encode(item, protocol, out);
    }
}

/// A status or error line; a line break inside one would end it early, so
/// each becomes a space.
fn line(out: &mut Vec<u8>, kind: u8, text: &[u8]) {
    out.push(kind);
    out.extend(text.iter().map(|byte| match byte {
        b'\r' | b'\n' => b' ',
        other => *other,
    }));
    out.extend_from_slice(b"\r\n");
}

fn length(count: usize) -> i64 {
    i64::try_from(count).unwrap_or(i64::MAX)
}

/// `integer` in decimal, with no allocation.
fn decimal(out: &mut Vec<u8>, integer: i64) {
    let mut digits = [0_u8; 20];
    let mut at = digits.len();
    let mut rest = integer.unsigned_abs();
    loop {
        at = at.saturating_sub(1);
        // A remainder of ten is below 256, so the byte holds it.
        digits[at] = b'0'.saturating_add(u8::try_from(rest % 10).unwrap_or(0));
        rest /= 10;
        if rest == 0 {
            break;
        }
    }
    if integer < 0 {
        out.push(b'-');
    }
    out.extend_from_slice(&digits[at..]);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn written(reply: &Reply, protocol: Protocol) -> String {
        let mut out = Vec::new();
        encode(reply, protocol, &mut out);
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn each_shape_is_written_in_each_version() {
        assert_eq!(written(&Reply::OK, Protocol::Two), "+OK\r\n");
        assert_eq!(
            written(&Reply::error("ERR a\r\nb"), Protocol::Two),
            "-ERR a  b\r\n"
        );
        assert_eq!(
            written(&Reply::Integer(i64::MIN), Protocol::Two),
            ":-9223372036854775808\r\n"
        );
        assert_eq!(written(&Reply::Integer(0), Protocol::Two), ":0\r\n");
        assert_eq!(written(&Reply::text("hi"), Protocol::Two), "$2\r\nhi\r\n");
        assert_eq!(written(&Reply::Nil, Protocol::Two), "$-1\r\n");
        assert_eq!(written(&Reply::Nil, Protocol::Three), "_\r\n");
        assert_eq!(written(&Reply::NilArray, Protocol::Two), "*-1\r\n");
        assert_eq!(written(&Reply::NilArray, Protocol::Three), "_\r\n");
        let map = Reply::Map(vec![(Reply::text("k"), Reply::Integer(1))]);
        assert_eq!(written(&map, Protocol::Two), "*2\r\n$1\r\nk\r\n:1\r\n");
        assert_eq!(written(&map, Protocol::Three), "%1\r\n$1\r\nk\r\n:1\r\n");
        let double = Reply::Double(b"2.5".to_vec());
        assert_eq!(written(&double, Protocol::Two), "$3\r\n2.5\r\n");
        assert_eq!(written(&double, Protocol::Three), ",2.5\r\n");
        let pairs = Reply::Pairs(vec![
            (Reply::text("a"), double.clone()),
            (Reply::text("a"), Reply::Integer(1)),
        ]);
        assert_eq!(
            written(&pairs, Protocol::Two),
            "*4\r\n$1\r\na\r\n$3\r\n2.5\r\n$1\r\na\r\n:1\r\n"
        );
        assert_eq!(
            written(&pairs, Protocol::Three),
            "*2\r\n*2\r\n$1\r\na\r\n,2.5\r\n*2\r\n$1\r\na\r\n:1\r\n"
        );
        assert_eq!(
            written(&Reply::Array(vec![Reply::Nil, Reply::OK]), Protocol::Two),
            "*2\r\n$-1\r\n+OK\r\n"
        );
        let push = Reply::Push(vec![Reply::text("message"), Reply::Integer(1)]);
        assert_eq!(
            written(&push, Protocol::Two),
            "*2\r\n$7\r\nmessage\r\n:1\r\n"
        );
        assert_eq!(
            written(&push, Protocol::Three),
            ">2\r\n$7\r\nmessage\r\n:1\r\n"
        );
        let mut shared = Vec::new();
        encode_push(
            &[b"message".as_slice(), b"news"],
            Protocol::Three,
            &mut shared,
        );
        assert_eq!(shared, b">2\r\n$7\r\nmessage\r\n$4\r\nnews\r\n");
        assert_eq!(
            written(
                &Reply::Several(vec![Reply::OK, Reply::Integer(2)]),
                Protocol::Two
            ),
            "+OK\r\n:2\r\n"
        );
    }
}
