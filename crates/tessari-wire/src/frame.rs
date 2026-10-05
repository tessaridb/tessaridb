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

mod codec;

use std::io::{Read, Write};

use tessari_encoding::{LogId, NODE_ID_LEN, Writer};
use tessari_types::{DatabaseId, NamespaceId, Reach, ShardId, TableId};

use crate::error::{Error, Result};
pub(crate) use codec::{
    put_bytes, put_log, put_reach, put_text, put_u32, put_u64, take_bytes, take_log, take_reach,
    take_text, take_u32, take_u64,
};

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
/// # Why this is 3
///
/// 3 since a refusal carries its class (ADR-0117 D3): a node at 3 starts the
/// refusal body it sends a client of 3 or more with the class byte.
///
/// # Why it was 2
///
/// 2 since the vault frame (ADR-0092 D2): a node at 2 answers
/// [`Kind::Vault`], and a client asks one only of a node that said 2 or more.
///
/// # Why it was 1
///
/// It was 0 through the two waves that built the redirect — the frame kind and
/// the client that can receive one — because **advertising a capability nothing
/// sends is worse than the gap**: a peer that believed this build could redirect
/// would have been believing something false. This build sends one, so the minor
/// moves with the sender and not with the frame.
pub(crate) const MINOR: u8 = 3;

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

/// The minor at which a node answers a [`Kind::Vault`] frame.
///
/// Asked by the client before it sends one: an older node closes the connection
/// on a kind it does not know, which would read as a network fault rather than
/// as the version gap it is.
pub(crate) const VAULT: u8 = 2;

/// This build's node answers the frame this build's client may send.
const _: () = assert!(MINOR >= VAULT);

/// The minor at which a client is sent a refusal's class (ADR-0117 D3).
///
/// The refusal body has no length prefix, so a client older than this would
/// read the byte as the first character of the message; it is sent the text
/// alone, exactly as before.
pub(crate) const CODES: u8 = 3;

/// This build's node classes the refusals this build's client reads.
const _: () = assert!(MINOR >= CODES);

/// The bytes a refusal body starts with when it carries a class: one, from
/// `0` (the node could not class it) to `9`. A message is UTF-8 prose and never
/// starts with a byte this low, which is what lets a reader tell a classed body
/// from one an older node — or the door, before any greeting — wrote as text.
pub(crate) const CLASS_BYTES: std::ops::RangeInclusive<u8> = 0..=9;

/// A refusal body for a peer of minor `theirs`: the class byte first when it
/// can read one, the store's own words after.
#[must_use]
pub(crate) fn refusal(theirs: u8, class: tessari_types::RefusalClass, text: &str) -> Vec<u8> {
    let mut body = Vec::with_capacity(text.len().saturating_add(1));
    if theirs >= CODES {
        body.push(class.byte());
    }
    body.extend_from_slice(text.as_bytes());
    body
}

/// A refusal body read back: its class, when it carried a readable one, and its
/// words.
///
/// `Some(None)` is a body that carried a class byte this build does not know, or
/// `0` — a class nobody could decide — and a reader treats it as not
/// retriable; `None` is a body with no class at all, from an older node.
#[must_use]
pub(crate) fn read_refusal(body: &[u8]) -> (Option<Option<tessari_types::RefusalClass>>, String) {
    match body.split_first() {
        Some((&first, rest)) if CLASS_BYTES.contains(&first) => (
            Some(tessari_types::RefusalClass::from_byte(first)),
            String::from_utf8_lossy(rest).into_owned(),
        ),
        _ => (None, String::from_utf8_lossy(body).into_owned()),
    }
}

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
    /// Unseal, seal or ask about the vault, the passphrase a field of its own.
    ///
    /// Numbered **17** because the peer link holds 14-16 (ADR-0092 D2). Its own
    /// kind rather than a script, so the passphrase is never statement text.
    Vault,
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
            Self::Vault => 17,
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
            17 => Some(Self::Vault),
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
    out.write_all(&header(tag, body)?)?;
    out.write_all(body)?;
    out.flush()?;
    Ok(())
}

/// The five bytes in front of `body`: its tag and its length.
///
/// One encoder for both the blocking and the async writers, so the ceiling is
/// refused on the way out by one piece of code whichever one is sending.
///
/// # Errors
///
/// Returns [`Error::TooLarge`] when the body is above the ceiling.
pub(crate) fn header(tag: u8, body: &[u8]) -> Result<[u8; 5]> {
    let length = u32::try_from(body.len()).unwrap_or(u32::MAX);
    if length > CEILING {
        return Err(Error::TooLarge { length });
    }
    let [a, b, c, d] = length.to_be_bytes();
    Ok([tag, a, b, c, d])
}

/// How long the body a header announces is, refused above the ceiling.
///
/// Shared by both readers for the reason [`header`] is shared by both writers,
/// and called **before** the body is allocated, which is the ceiling's purpose.
///
/// # Errors
///
/// Returns [`Error::TooLarge`] when the declared length is above the ceiling.
pub(crate) fn announced(header: &[u8; 5]) -> Result<usize> {
    let length = u32::from_be_bytes([header[1], header[2], header[3], header[4]]);
    if length > CEILING {
        return Err(Error::TooLarge { length });
    }
    Ok(usize::try_from(length).unwrap_or(0))
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

    // Checked before the allocation, which is the whole point of the ceiling.
    let mut body = vec![0_u8; announced(&header)?];
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
mod tests;
