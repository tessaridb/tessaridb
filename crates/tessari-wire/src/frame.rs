//! The frames, and the one rule that makes reading them safe.
//!
//! # A length from a stranger is not a promise
//!
//! Every frame declares its own size, and a server that allocated whatever a
//! client declared would be one packet away from being out of memory — the
//! oldest denial of service there is, and one that needs no cleverness to
//! perform. So a declared length above [`CEILING`] is refused **before anything
//! is allocated**, and the connection closes.
//!
//! The ceiling is generous rather than tuned: a script is text somebody wrote,
//! and an answer is bounded by what the store held. It exists to be an absurdity
//! check, not a quota.
//!
//! # Why the kind comes first
//!
//! A reader that has to parse a body to learn what it was reading cannot refuse
//! a body it does not want. The kind is one byte, and an unknown one closes the
//! connection rather than being skipped: a protocol that ignores what it does
//! not understand is one where a version mismatch looks like silence.

use std::io::{Read, Write};

use crate::error::{Error, Result};

/// What every connection says first, in both directions.
pub(crate) const HELLO: &[u8; 4] = b"TESS";

/// The protocol's version.
///
/// Checked on both sides at the hello, so a mismatch is one clear refusal at the
/// start rather than a decode failure somewhere in the middle that reads like
/// corruption.
///
/// It moved to 2 when a records answer began carrying the names of the tables
/// its references point at, and to 3 when a request began carrying the values
/// its script's parameters bind to. Both are changes to the layout of a body,
/// which is exactly the kind of change this byte exists for: a version that does
/// not move when the layout does teaches a reader that the number is decoration,
/// and the next mismatch arrives as corruption in the middle of a frame.
pub(crate) const VERSION: u8 = 3;

/// The largest frame this build will read.
///
/// Sixteen mebibytes: far above any script somebody types and any answer this
/// store has been measured returning, and far below what a server can be asked
/// to allocate by accident.
pub(crate) const CEILING: u32 = 16 * 1024 * 1024;

/// What a frame is.
///
/// Numbered explicitly and never renumbered, for the reason the key-kind table
/// is: a byte that once meant one thing and later means another cannot be asked
/// about after the fact — here, by a client of a different build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub(crate) enum Kind {
    /// A script to run, with optional credentials.
    Request,
    /// One outcome per statement.
    Answer,
    /// The store refused, and said why.
    Refusal,
    /// Follow the changes from a position onward.
    ///
    /// The client asks once; everything after it comes the other way without
    /// being asked for, which is the reason this protocol has kinds at all.
    Subscribe,
    /// One change, sent because it happened.
    Change,
}

impl Kind {
    pub(crate) const fn tag(self) -> u8 {
        match self {
            Self::Request => 1,
            Self::Answer => 2,
            Self::Refusal => 3,
            Self::Subscribe => 4,
            Self::Change => 5,
        }
    }

    pub(crate) const fn from_tag(tag: u8) -> Option<Self> {
        match tag {
            1 => Some(Self::Request),
            2 => Some(Self::Answer),
            3 => Some(Self::Refusal),
            4 => Some(Self::Subscribe),
            5 => Some(Self::Change),
            // 6 and above stay unclaimed, and an unknown kind still closes the
            // connection rather than being skipped: a protocol that ignores what
            // it does not understand is one where a version mismatch looks like
            // silence.
            _ => None,
        }
    }
}

/// Write one frame.
///
/// # Errors
///
/// Returns an error when the stream fails, or when the body is above the
/// ceiling — refused on the way out as well as in, because a server that emits
/// what it would refuse to read has two protocols.
pub(crate) fn write(out: &mut impl Write, kind: Kind, body: &[u8]) -> Result<()> {
    let length = u32::try_from(body.len()).unwrap_or(u32::MAX);
    if length > CEILING {
        return Err(Error::TooLarge { length });
    }
    out.write_all(&[kind.tag()])?;
    out.write_all(&length.to_be_bytes())?;
    out.write_all(body)?;
    out.flush()?;
    Ok(())
}

/// Read one frame, or `None` when the peer hung up cleanly between frames.
///
/// # Errors
///
/// Returns [`Error::TooLarge`] before allocating when the declared length is
/// above the ceiling, [`Error::UnknownFrame`] for a kind this build does not
/// have, and the stream's own failure otherwise.
pub(crate) fn read(input: &mut impl Read) -> Result<Option<(Kind, Vec<u8>)>> {
    let mut header = [0_u8; 5];
    let mut held = 0;
    while held < header.len() {
        let Some(slot) = header.get_mut(held..) else {
            break;
        };
        let read = input.read(slot)?;
        if read == 0 {
            // Nothing at all is a clean goodbye; a partial header is not.
            return if held == 0 {
                Ok(None)
            } else {
                Err(Error::Truncated)
            };
        }
        held = held.saturating_add(read);
    }

    let Some(kind) = Kind::from_tag(header[0]) else {
        return Err(Error::UnknownFrame { tag: header[0] });
    };
    let length = u32::from_be_bytes([header[1], header[2], header[3], header[4]]);
    // Checked before the allocation, which is the whole point of the ceiling.
    if length > CEILING {
        return Err(Error::TooLarge { length });
    }
    let mut body = vec![0_u8; usize::try_from(length).unwrap_or(0)];
    input.read_exact(&mut body).map_err(|_| Error::Truncated)?;
    Ok(Some((kind, body)))
}

/// Say hello, and hear one back.
///
/// # Errors
///
/// Returns [`Error::NotThisProtocol`] when the greeting is not one, and
/// [`Error::WrongVersion`] when it is one this build does not speak.
pub(crate) fn greet(stream: &mut (impl Read + Write)) -> Result<()> {
    stream.write_all(HELLO)?;
    stream.write_all(&[VERSION])?;
    stream.flush()?;

    let mut said = [0_u8; 5];
    stream
        .read_exact(&mut said)
        .map_err(|_| Error::NotThisProtocol)?;
    if said.get(..4) != Some(HELLO.as_slice()) {
        return Err(Error::NotThisProtocol);
    }
    let found = said[4];
    if found != VERSION {
        return Err(Error::WrongVersion {
            found,
            supported: VERSION,
        });
    }
    Ok(())
}

/// A length-prefixed string, the shape every text in a body takes.
pub(crate) fn put_text(into: &mut Vec<u8>, text: &str) {
    let length = u32::try_from(text.len()).unwrap_or(u32::MAX);
    into.extend_from_slice(&length.to_be_bytes());
    into.extend_from_slice(text.as_bytes());
}

/// Read one back, and how much of the buffer it used.
pub(crate) fn take_text(from: &[u8], at: usize) -> Result<(String, usize)> {
    let (bytes, next) = take_bytes(from, at)?;
    let text = String::from_utf8(bytes).map_err(|_| Error::Malformed)?;
    Ok((text, next))
}

/// A length-prefixed byte string.
pub(crate) fn put_bytes(into: &mut Vec<u8>, bytes: &[u8]) {
    let length = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
    into.extend_from_slice(&length.to_be_bytes());
    into.extend_from_slice(bytes);
}

/// Read one back.
pub(crate) fn take_bytes(from: &[u8], at: usize) -> Result<(Vec<u8>, usize)> {
    let end = at.checked_add(4).ok_or(Error::Malformed)?;
    let header = from.get(at..end).ok_or(Error::Malformed)?;
    let length = usize::try_from(u32::from_be_bytes([
        *header.first().ok_or(Error::Malformed)?,
        *header.get(1).ok_or(Error::Malformed)?,
        *header.get(2).ok_or(Error::Malformed)?,
        *header.get(3).ok_or(Error::Malformed)?,
    ]))
    .unwrap_or(0);
    let stop = end.checked_add(length).ok_or(Error::Malformed)?;
    let bytes = from.get(end..stop).ok_or(Error::Malformed)?;
    Ok((bytes.to_vec(), stop))
}

/// A `u32`, for counts.
pub(crate) fn put_u32(into: &mut Vec<u8>, value: u32) {
    into.extend_from_slice(&value.to_be_bytes());
}

/// A `u64`, for a position in the log.
pub(crate) fn put_u64(into: &mut Vec<u8>, value: u64) {
    into.extend_from_slice(&value.to_be_bytes());
}

/// Read one back.
pub(crate) fn take_u64(from: &[u8], at: usize) -> Result<(u64, usize)> {
    let end = at.checked_add(8).ok_or(Error::Malformed)?;
    let bytes = from.get(at..end).ok_or(Error::Malformed)?;
    let mut held = [0_u8; 8];
    held.copy_from_slice(bytes);
    Ok((u64::from_be_bytes(held), end))
}

/// Read one back.
pub(crate) fn take_u32(from: &[u8], at: usize) -> Result<(u32, usize)> {
    let end = at.checked_add(4).ok_or(Error::Malformed)?;
    let bytes = from.get(at..end).ok_or(Error::Malformed)?;
    Ok((
        u32::from_be_bytes([
            *bytes.first().ok_or(Error::Malformed)?,
            *bytes.get(1).ok_or(Error::Malformed)?,
            *bytes.get(2).ok_or(Error::Malformed)?,
            *bytes.get(3).ok_or(Error::Malformed)?,
        ]),
        end,
    ))
}

/// A reader and a writer over one socket, for the greeting.
///
/// Here rather than beside either end, because both ends greet and the greeting
/// is the one moment a connection needs both directions on one object. After it
/// they are used independently, which is what lets a change be written while a
/// read is waiting.
pub(crate) struct Duplex<'a, R, W> {
    pub(crate) reader: &'a mut R,
    pub(crate) writer: &'a mut W,
}

impl<R: std::io::Read, W: std::io::Write> std::io::Read for Duplex<'_, R, W> {
    fn read(&mut self, into: &mut [u8]) -> std::io::Result<usize> {
        self.reader.read(into)
    }
}

impl<R: std::io::Read, W: std::io::Write> std::io::Write for Duplex<'_, R, W> {
    fn write(&mut self, from: &[u8]) -> std::io::Result<usize> {
        self.writer.write(from)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.writer.flush()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic)]

    use std::io::Cursor;

    use super::{CEILING, Kind, put_text, read, take_text, write};
    use crate::error::Error;

    #[test]
    fn a_frame_survives_the_round_trip() {
        let mut held = Vec::new();
        write(&mut held, Kind::Request, b"a script").expect("written");
        let mut reader = Cursor::new(held);
        let (kind, body) = read(&mut reader).expect("read").expect("a frame");
        assert_eq!(kind, Kind::Request);
        assert_eq!(body, b"a script");
        // And the stream is then cleanly at its end.
        assert!(read(&mut reader).expect("read").is_none());
    }

    #[test]
    fn a_declared_length_above_the_ceiling_is_refused_before_anything_is_allocated() {
        // The oldest denial of service there is, and it needs no cleverness: a
        // five-byte header claiming four gibibytes.
        let mut held = vec![Kind::Request.tag()];
        held.extend_from_slice(&CEILING.saturating_add(1).to_be_bytes());
        let refused = read(&mut Cursor::new(held)).expect_err("a refusal");
        assert!(matches!(refused, Error::TooLarge { .. }), "{refused}");
    }

    #[test]
    fn a_kind_this_build_does_not_have_closes_rather_than_being_skipped() {
        // A protocol that ignores what it does not understand is one where a
        // version mismatch looks like silence.
        let held = vec![99, 0, 0, 0, 0];
        let refused = read(&mut Cursor::new(held)).expect_err("a refusal");
        assert!(
            matches!(refused, Error::UnknownFrame { tag: 99 }),
            "{refused}"
        );
    }

    #[test]
    fn a_header_that_stops_halfway_is_a_truncation_and_not_a_goodbye() {
        let refused = read(&mut Cursor::new(vec![1, 0])).expect_err("a refusal");
        assert!(matches!(refused, Error::Truncated), "{refused}");
    }

    #[test]
    fn a_body_shorter_than_its_own_header_says_is_a_truncation() {
        let mut held = vec![Kind::Answer.tag()];
        held.extend_from_slice(&100_u32.to_be_bytes());
        held.extend_from_slice(b"not a hundred bytes");
        let refused = read(&mut Cursor::new(held)).expect_err("a refusal");
        assert!(matches!(refused, Error::Truncated), "{refused}");
    }

    #[test]
    fn text_round_trips_including_nothing_at_all() {
        for held in ["a script", "", "unicode — ok"] {
            let mut body = Vec::new();
            put_text(&mut body, held);
            let (read_back, used) = take_text(&body, 0).expect("text");
            assert_eq!(read_back, held);
            assert_eq!(used, body.len());
        }
    }

    #[test]
    fn a_body_that_promises_more_than_it_holds_is_malformed_rather_than_panicking() {
        // Every reader in this module is index-checked, because a body is
        // whatever a stranger sent.
        let mut body = Vec::new();
        put_text(&mut body, "abc");
        for cut in 0..body.len() {
            assert!(take_text(&body[..cut], 0).is_err(), "a cut at {cut} parsed");
        }
    }
}
