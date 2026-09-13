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

/// The protocol's major version — the half a mismatch is refused on.
///
/// Checked on both sides at the hello, so a mismatch is one clear refusal at the
/// start rather than a decode failure somewhere in the middle that reads like
/// corruption.
///
/// # Why this is 1 and not 4
///
/// A single version byte lived here and moved twice during development — to 2
/// when a records answer began carrying table names, to 3 when a request began
/// carrying its parameters' values. Both moves were correct by the rule the byte
/// carried and pointless by purpose: **a version exists to refuse a mismatch
/// between builds that are actually in somebody's hands**, and before a release
/// there are none. Left alone it would have made the first public version 3 with
/// two unreachable predecessors, so the specification retired it and the
/// published protocol starts at 1.0.
pub(crate) const MAJOR: u8 = 1;

/// The protocol's minor version — the half a mismatch is *not* refused on.
///
/// A differing minor means the two sides agree about frames and about every
/// value, and the newer one merely knows more outcome kinds — which the older
/// one steps over by the length in front of each (see `message`). So the peer's
/// minor is kept rather than compared, and it decides exactly one thing: what
/// this side may *send* to an older peer.
///
/// This is why a new outcome kind is a minor change and a new value type is a
/// major one: a value nested inside an array carries no length of its own, so an
/// unknown one cannot be stepped over.
///
/// # Why this is 1
///
/// It was 0 through the two waves that built the redirect — the frame kind and
/// the client that can receive one — because **advertising a capability nothing
/// sends is worse than the gap**: a peer that believed this build could redirect
/// would have been believing something false. This build sends one, so the minor
/// moves with the sender and not with the frame.
pub(crate) const MINOR: u8 = 1;

/// The minor at which a peer can be sent a [`Kind::Elsewhere`] frame.
///
/// Named rather than written as `1` at the comparison, because the number alone
/// cannot say what it is a threshold *for*, and the next thing gated on a minor
/// will need its own name beside this one rather than a second bare literal.
pub(crate) const REDIRECTS: u8 = 1;

/// This build's own client must be able to read what this build's node sends.
///
/// The two constants above answer different questions — what we speak, and what
/// a peer must speak to be sent a redirect — so nothing but this line notices if
/// they drift apart. A build advertising a minor below the one its own redirect
/// needs would refuse to send a frame it can read perfectly well, and every test
/// in the crate would pass.
///
/// Checked at compile time rather than in a test, because a threshold that is
/// wrong is wrong for every caller at once and there is nothing to gain by
/// finding out at run time.
const _: () = assert!(MINOR >= REDIRECTS);

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
    /// This read belongs somewhere else, and this is where.
    ///
    /// Numbered **13** rather than 6, which is where 1-5 leaves off, because the
    /// peer link claims 6-12 out of the same byte. See
    /// [`crate::peer::PeerFrame`] for what the two spaces owe each other — the
    /// rule is that neither reader accepts the other's tags, and a contiguous
    /// range was only ever a convenient way to say so.
    Elsewhere,
}

impl Kind {
    pub(crate) const fn tag(self) -> u8 {
        match self {
            Self::Request => 1,
            Self::Answer => 2,
            Self::Refusal => 3,
            Self::Subscribe => 4,
            Self::Change => 5,
            Self::Elsewhere => 13,
        }
    }

    pub(crate) const fn from_tag(tag: u8) -> Option<Self> {
        match tag {
            1 => Some(Self::Request),
            2 => Some(Self::Answer),
            3 => Some(Self::Refusal),
            4 => Some(Self::Subscribe),
            5 => Some(Self::Change),
            13 => Some(Self::Elsewhere),
            // 6-12 belong to the peer link and are refused here on purpose, so a
            // peer frame arriving on the client port closes the connection
            // instead of being misread. Everything else is simply unclaimed, and
            // an unknown kind closes the connection rather than being skipped: a
            // protocol that ignores what it does not understand is one where a
            // version mismatch looks like silence.
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
    write_tagged(out, kind.tag(), body)
}

/// Write one frame under a tag this module does not interpret.
///
/// The peer link has its own tag space (see [`crate::peer::PeerFrame`]) and the
/// same header, so it shares the ceiling rather than carrying a second copy of
/// it — a second copy is how two limits come to disagree.
pub(crate) fn write_tagged(out: &mut impl Write, tag: u8, body: &[u8]) -> Result<()> {
    let length = u32::try_from(body.len()).unwrap_or(u32::MAX);
    if length > CEILING {
        return Err(Error::TooLarge { length });
    }
    out.write_all(&[tag])?;
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
    let Some((tag, body)) = read_tagged(input)? else {
        return Ok(None);
    };
    let Some(kind) = Kind::from_tag(tag) else {
        return Err(Error::UnknownFrame { tag });
    };
    Ok(Some((kind, body)))
}

/// Read one frame without deciding what its tag means.
///
/// The tag is handed back raw because the peer link's tags are not this
/// module's to know; the ceiling, the header shape and the clean-goodbye rule
/// are, and they are the parts worth having in one place.
pub(crate) fn read_tagged(input: &mut impl Read) -> Result<Option<(u8, Vec<u8>)>> {
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

    let length = u32::from_be_bytes([header[1], header[2], header[3], header[4]]);
    // Checked before the allocation, which is the whole point of the ceiling.
    if length > CEILING {
        return Err(Error::TooLarge { length });
    }
    let mut body = vec![0_u8; usize::try_from(length).unwrap_or(0)];
    input.read_exact(&mut body).map_err(|_| Error::Truncated)?;
    Ok(Some((header[0], body)))
}

/// Say hello, and hear one back.
///
/// # Errors
///
/// Returns [`Error::NotThisProtocol`] when the greeting is not one, and
/// [`Error::WrongVersion`] when it is one this build does not speak.
pub(crate) fn greet(stream: &mut (impl Read + Write)) -> Result<u8> {
    stream.write_all(HELLO)?;
    stream.write_all(&[MAJOR, MINOR])?;
    stream.flush()?;

    // The magic is judged on its own four bytes, before the version bytes are
    // read at all. A peer that is not a node owes nothing — it may send three
    // bytes of an HTTP request line and hang up — and a reader that waited for
    // all six first would report that as a truncated stream, which sends
    // whoever reads the error to the network when the answer is that the
    // address is wrong.
    let mut magic = [0_u8; 4];
    stream
        .read_exact(&mut magic)
        .map_err(|_| Error::NotThisProtocol)?;
    if &magic != HELLO {
        return Err(Error::NotThisProtocol);
    }

    let mut version = [0_u8; 2];
    stream
        .read_exact(&mut version)
        .map_err(|_| Error::Truncated)?;
    let found = version[0];
    if found != MAJOR {
        return Err(Error::WrongVersion {
            found,
            supported: MAJOR,
        });
    }
    // The peer's minor, returned rather than discarded: it is the only thing
    // that decides what this side may send to an older peer.
    Ok(version[1])
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

    use super::{CEILING, Kind, greet, put_text, read, take_text, write};
    use crate::error::Error;

    /// What a node must put on the wire, written as bytes rather than built from
    /// this module's own constants.
    ///
    /// Interpolating `MAJOR` and `MINOR` here would make the test agree with
    /// whatever they say, which is not a check — and agreeing with itself is
    /// exactly how this drifted to a five-byte greeting at version 3 while every
    /// test in the crate passed. The specification says six bytes; six bytes are
    /// written here by hand.
    ///
    /// The last byte moved from 0 to 1 in W267, and this test is the reason the
    /// move was noticed at all: the minor bump was made in the engine, and the
    /// **specification** is what had to be changed to match. Written out, the
    /// constant makes a protocol change fail in this crate until somebody has
    /// been to `spec/protocol-v1.md` §2.3 and §3.1 and changed the document a
    /// third-party client is written against.
    const SPECIFIED_GREETING: [u8; 6] = [b'T', b'E', b'S', b'S', 1, 1];

    /// A peer: what it will say, and what it hears.
    ///
    /// A `Cursor` cannot stand in for one here — `greet` writes before it reads,
    /// and a cursor's write would land on top of the bytes the read is about to
    /// take.
    struct Peer {
        says: Cursor<Vec<u8>>,
        heard: Vec<u8>,
    }

    impl Peer {
        fn saying(bytes: &[u8]) -> Self {
            Self {
                says: Cursor::new(bytes.to_vec()),
                heard: Vec::new(),
            }
        }
    }

    impl std::io::Read for Peer {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            self.says.read(buffer)
        }
    }

    impl std::io::Write for Peer {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            self.heard.extend_from_slice(buffer);
            Ok(buffer.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn the_greeting_this_node_sends_is_the_six_bytes_the_specification_names() {
        let mut peer = Peer::saying(&SPECIFIED_GREETING);
        let minor = greet(&mut peer).expect("a node greets a node");
        assert_eq!(
            peer.heard, SPECIFIED_GREETING,
            "what this node puts on the wire is not what the specification says"
        );
        assert_eq!(minor, 1, "the peer's minor is kept, not discarded");
    }

    #[test]
    fn a_peer_that_is_not_a_node_is_named_as_such_and_not_as_a_short_read() {
        // Three bytes of an HTTP request line, then nothing. The magic is judged
        // on its own four bytes, so this is `NotThisProtocol` — which sends the
        // reader to the address — and not `Truncated`, which would send them to
        // the network, where there is nothing to find.
        let mut peer = Peer::saying(b"GET");
        assert!(matches!(greet(&mut peer), Err(Error::NotThisProtocol)));
    }

    #[test]
    fn a_differing_major_is_refused_and_carries_both_numbers() {
        let mut peer = Peer::saying(&[b'T', b'E', b'S', b'S', 9, 0]);
        match greet(&mut peer) {
            Err(Error::WrongVersion { found, supported }) => {
                assert_eq!(found, 9);
                assert_eq!(supported, 1);
            }
            other => panic!("expected a refusal naming both versions, got {other:?}"),
        }
    }

    #[test]
    fn a_differing_minor_is_not_a_refusal() {
        // The half the specification says must be tolerated: the two sides agree
        // about frames and about every value, and the newer one merely knows
        // more outcome kinds, which the older steps over by their lengths.
        let mut peer = Peer::saying(&[b'T', b'E', b'S', b'S', 1, 7]);
        assert_eq!(greet(&mut peer).expect("tolerated"), 7);
    }

    #[test]
    fn a_greeting_that_stops_after_the_magic_is_a_truncation() {
        // Four correct bytes and then nothing is a node that died mid-greeting,
        // which is a transport problem and is reported as one.
        let mut peer = Peer::saying(b"TESS");
        assert!(matches!(greet(&mut peer), Err(Error::Truncated)));
    }

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
