#![allow(clippy::panic)]

use std::io::Cursor;

use super::{CEILING, Kind, greet, put_reach, put_text, read, take_reach, take_text, write};
use crate::error::Error;

#[test]
fn a_shard_home_crosses_the_wire_and_the_older_three_keep_nine_bytes() {
    use tessari_types::{DatabaseId, NamespaceId, Reach, ShardId, TableId};
    let shard = Reach::Shard(
        NamespaceId::new(1),
        DatabaseId::new(2),
        TableId::new(3),
        ShardId::new(4),
    );
    let mut bytes = Vec::new();
    put_reach(&mut bytes, shard);
    assert_eq!(
        bytes,
        vec![3, 0, 0, 0, 1, 0, 0, 0, 2, 0, 0, 0, 3, 0, 0, 0, 4]
    );
    assert!(matches!(take_reach(&bytes, 0), Ok((read, 17)) if read == shard));
    let mut older = Vec::new();
    put_reach(
        &mut older,
        Reach::Database(NamespaceId::new(1), DatabaseId::new(2)),
    );
    assert_eq!(older, vec![2, 0, 0, 0, 1, 0, 0, 0, 2]);
    // A shard numbered zero is not one, on the wire as in the catalog.
    let mut zero = bytes.clone();
    zero[16] = 0;
    assert!(matches!(take_reach(&zero, 0), Err(Error::Malformed)));
}

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
const SPECIFIED_GREETING: [u8; 6] = [b'T', b'E', b'S', b'S', 1, 2];

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
    assert_eq!(minor, 2, "the peer's minor is kept, not discarded");
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
