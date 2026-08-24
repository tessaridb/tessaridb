//! One WebSocket frame, read and written.
//!
//! RFC 6455 §5. A two-byte header, an optional extended length, a four-byte mask
//! when the sender is a client, and the payload. The whole format fits on a page
//! and this file is that page.
//!
//! # What is refused, and why refusing is the feature
//!
//! Three of the four ways a hand-written implementation goes wrong are decided
//! here rather than left to a caller.
//!
//! A **client frame must be masked**. The requirement exists to stop a socket
//! being used to smuggle attacker-chosen bytes past a cache that only reads the
//! start of a stream, and a server that quietly accepts an unmasked frame
//! removes that protection for everybody on the path. So it is a protocol error
//! and the connection ends.
//!
//! A **declared length is checked before it is believed**. The header says how
//! long the payload is; a reader that allocates on that number allocates
//! whatever a stranger asked for.
//!
//! A **control frame is never fragmented and never longer than 125 bytes** —
//! RFC 6455 §5.5. Both are stated as constraints on the sender, which means a
//! reader that does not check them is the one that breaks.

use std::io::{Read, Write};

use tessari_constants::SOCKET_MAX_FRAME_BYTES;

/// What a frame is for.
///
/// The reserved opcodes are deliberately not represented: a frame carrying one
/// is a protocol error, and giving it a name here would invite code that handles
/// it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Opcode {
    /// The continuation of a message whose first frame said more was coming.
    Continuation,
    /// A whole message, or its first fragment, as UTF-8 text.
    Text,
    /// The same, as bytes.
    Binary,
    /// The sender is closing, and says why.
    Close,
    /// Answer me, so I know you are there.
    Ping,
    /// The answer to a ping.
    Pong,
}

impl Opcode {
    /// The opcode `tag` stands for, or nothing when it is reserved.
    const fn read(tag: u8) -> Option<Self> {
        match tag {
            0 => Some(Self::Continuation),
            1 => Some(Self::Text),
            2 => Some(Self::Binary),
            8 => Some(Self::Close),
            9 => Some(Self::Ping),
            10 => Some(Self::Pong),
            _ => None,
        }
    }

    /// The four bits this opcode is written as.
    const fn tag(self) -> u8 {
        match self {
            Self::Continuation => 0,
            Self::Text => 1,
            Self::Binary => 2,
            Self::Close => 8,
            Self::Ping => 9,
            Self::Pong => 10,
        }
    }

    /// Whether this frame is about the connection rather than its content.
    ///
    /// Control frames may arrive **between** the fragments of a message, which
    /// is the rule a reassembly loop written as "read until FIN" gets wrong.
    pub(crate) const fn is_control(self) -> bool {
        matches!(self, Self::Close | Self::Ping | Self::Pong)
    }
}

/// One frame off the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Frame {
    /// Whether this is the last frame of its message.
    pub fin: bool,
    /// What the frame is for.
    pub opcode: Opcode,
    /// The payload, already unmasked.
    pub payload: Vec<u8>,
}

/// Why reading stopped.
///
/// Each variant carries the close code the connection should end with, because
/// deciding that at the point the fault is detected is what keeps the codes from
/// drifting into "1002 for everything".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Broken {
    /// The client went away without a close frame. Nothing to answer.
    Ended,
    /// The socket failed. Nothing to answer.
    Failed,
    /// The frame breaks the protocol: unmasked, a reserved opcode, a fragmented
    /// control frame, or a continuation with nothing to continue.
    Protocol,
    /// The frame or the message it belongs to is larger than this node reads.
    TooLarge,
}

impl Broken {
    /// The close code to end with, or nothing when there is nobody to tell.
    pub(crate) const fn code(self) -> Option<u16> {
        match self {
            Self::Ended | Self::Failed => None,
            Self::Protocol => Some(1002),
            Self::TooLarge => Some(1009),
        }
    }
}

/// Read one frame from a client.
///
/// # Errors
///
/// Returns why reading stopped — a clean end, a failed socket, a protocol fault,
/// or a frame this node refuses to hold.
pub(crate) fn read(reader: &mut impl Read) -> Result<Frame, Broken> {
    let mut header = [0u8; 2];
    match reader.read_exact(&mut header) {
        Ok(()) => {}
        Err(why) if why.kind() == std::io::ErrorKind::UnexpectedEof => return Err(Broken::Ended),
        Err(_) => return Err(Broken::Failed),
    }

    let fin = header[0] & 0b1000_0000 != 0;
    // The three reserved bits are only meaningful under an extension, and this
    // node declines every extension in the handshake — so a peer setting one is
    // using something it was told it could not have.
    if header[0] & 0b0111_0000 != 0 {
        return Err(Broken::Protocol);
    }
    let opcode = Opcode::read(header[0] & 0b0000_1111).ok_or(Broken::Protocol)?;
    let masked = header[1] & 0b1000_0000 != 0;
    if !masked {
        return Err(Broken::Protocol);
    }

    let declared = usize::from(header[1] & 0b0111_1111);
    let length = match declared {
        126 => extended::<2>(reader)?,
        127 => extended::<8>(reader)?,
        short => short,
    };
    // Checked against the declaration, before a byte is reserved for it.
    if length > SOCKET_MAX_FRAME_BYTES {
        return Err(Broken::TooLarge);
    }
    // §5.5: a control frame carries at most 125 bytes and is never fragmented.
    // Both are stated as sender rules, so a reader that skips them is the one
    // that breaks.
    if opcode.is_control() && (length > 125 || !fin) {
        return Err(Broken::Protocol);
    }

    let mut mask = [0u8; 4];
    take(reader, &mut mask)?;
    let mut payload = vec![0u8; length];
    take(reader, &mut payload)?;
    for (index, byte) in payload.iter_mut().enumerate() {
        // `% 4` over a four-byte key, which is the whole of the masking rule.
        *byte ^= mask[index % 4];
    }

    Ok(Frame {
        fin,
        opcode,
        payload,
    })
}

/// Read an extended length of `BYTES` bytes.
fn extended<const BYTES: usize>(reader: &mut impl Read) -> Result<usize, Broken> {
    let mut raw = [0u8; BYTES];
    take(reader, &mut raw)?;
    let mut length: u64 = 0;
    for byte in raw {
        // Big-endian, assembled rather than cast, so the two widths share one
        // path and neither needs a lossy conversion.
        length = (length << 8) | u64::from(byte);
    }
    // A length that does not fit a `usize` is by definition past the ceiling.
    usize::try_from(length).map_err(|_| Broken::TooLarge)
}

/// Fill `buffer`, treating a short read as the connection ending.
fn take(reader: &mut impl Read, buffer: &mut [u8]) -> Result<(), Broken> {
    match reader.read_exact(buffer) {
        Ok(()) => Ok(()),
        Err(why) if why.kind() == std::io::ErrorKind::UnexpectedEof => Err(Broken::Ended),
        Err(_) => Err(Broken::Failed),
    }
}

/// Write one frame to a client.
///
/// Server frames are never masked — RFC 6455 §5.1 forbids it, and a browser
/// closes the connection on one that is.
///
/// # Errors
///
/// Returns [`Broken::Failed`] when the socket cannot be written.
pub(crate) fn write(
    writer: &mut impl Write,
    fin: bool,
    opcode: Opcode,
    payload: &[u8],
) -> Result<(), Broken> {
    let mut header = Vec::with_capacity(10);
    header.push(if fin {
        0b1000_0000 | opcode.tag()
    } else {
        opcode.tag()
    });
    // The three length forms, chosen by what the payload needs rather than by a
    // preference: a peer is entitled to refuse a longer form than necessary.
    match payload.len() {
        short if short < 126 => {
            let Ok(byte) = u8::try_from(short) else {
                return Err(Broken::Failed);
            };
            header.push(byte);
        }
        medium if medium <= usize::from(u16::MAX) => {
            let Ok(length) = u16::try_from(medium) else {
                return Err(Broken::Failed);
            };
            header.push(126);
            header.extend_from_slice(&length.to_be_bytes());
        }
        long => {
            let Ok(length) = u64::try_from(long) else {
                return Err(Broken::Failed);
            };
            header.push(127);
            header.extend_from_slice(&length.to_be_bytes());
        }
    }
    if writer.write_all(&header).is_err() || writer.write_all(payload).is_err() {
        return Err(Broken::Failed);
    }
    // Flushed here rather than by the caller: a frame nobody sent is a frame the
    // peer waits for, and the peer in the interesting case is a browser holding
    // a socket open on a pong it will never see.
    writer.flush().map_err(|_| Broken::Failed)
}

/// Write a close frame carrying `code`.
///
/// # Errors
///
/// Returns [`Broken::Failed`] when the socket cannot be written.
pub(crate) fn close(writer: &mut impl Write, code: u16) -> Result<(), Broken> {
    write(writer, true, Opcode::Close, &code.to_be_bytes())
}

/// The code a close frame carries, or `1000` when it carries nothing.
///
/// A close with an empty payload is legal and means a normal end, so it is
/// echoed as one rather than treated as malformed.
pub(crate) fn closed_with(payload: &[u8]) -> u16 {
    match payload {
        [high, low, ..] => u16::from_be_bytes([*high, *low]),
        _ => 1000,
    }
}

#[cfg(test)]
mod tests {
    use super::{Broken, Frame, Opcode, close, closed_with, read, write};

    /// A frame as a **client** sends it: masked, with the mask applied.
    fn masked(fin: bool, opcode: u8, payload: &[u8]) -> Vec<u8> {
        let mask = [0x37u8, 0xfa, 0x21, 0x3d];
        let mut out = vec![if fin { 0b1000_0000 | opcode } else { opcode }];
        match payload.len() {
            short if short < 126 => out.push(0b1000_0000 | u8::try_from(short).unwrap_or(0)),
            medium => {
                out.push(0b1000_0000 | 126);
                out.extend_from_slice(&u16::try_from(medium).unwrap_or(0).to_be_bytes());
            }
        }
        out.extend_from_slice(&mask);
        out.extend(
            payload
                .iter()
                .enumerate()
                .map(|(index, byte)| byte ^ mask[index % 4]),
        );
        out
    }

    #[test]
    fn a_masked_client_frame_is_unmasked() {
        let raw = masked(true, 1, b"hello");
        let frame = read(&mut raw.as_slice()).expect("a well-formed frame");
        assert_eq!(
            frame,
            Frame {
                fin: true,
                opcode: Opcode::Text,
                payload: b"hello".to_vec(),
            },
            "the payload was not unmasked with the key the client sent"
        );
    }

    #[test]
    fn an_unmasked_client_frame_is_a_protocol_error() {
        // The same frame with the mask bit cleared and the payload in the clear.
        let raw = [0b1000_0001u8, 5, b'h', b'e', b'l', b'l', b'o'];
        assert_eq!(
            read(&mut raw.as_slice()),
            Err(Broken::Protocol),
            "an unmasked client frame was accepted, which removes the protection \
             masking exists for"
        );
    }

    #[test]
    fn the_two_length_forms_round_trip() {
        for length in [0usize, 1, 125, 126, 127, 1000] {
            let payload = vec![b'z'; length];
            let raw = masked(true, 2, &payload);
            let frame = read(&mut raw.as_slice()).expect("a well-formed frame");
            assert_eq!(
                frame.payload.len(),
                length,
                "length {length} did not survive"
            );
            assert_eq!(frame.opcode, Opcode::Binary, "length {length}");
        }
    }

    #[test]
    fn a_declared_length_past_the_ceiling_is_refused_before_it_is_read() {
        // A header claiming four gigabytes, followed by nothing at all. A reader
        // that allocates on the declaration would ask for four gigabytes here;
        // one that reads first would block. Neither happens.
        let raw = [0b1000_0010u8, 0b1111_1111, 0, 0, 0, 1, 0, 0, 0, 0];
        assert_eq!(
            read(&mut raw.as_slice()),
            Err(Broken::TooLarge),
            "a declared length was believed instead of checked"
        );
    }

    #[test]
    fn a_control_frame_may_be_neither_long_nor_fragmented() {
        let long = masked(true, 9, &[b'p'; 126]);
        assert_eq!(
            read(&mut long.as_slice()),
            Err(Broken::Protocol),
            "a control frame longer than 125 bytes was accepted"
        );
        let split = masked(false, 9, b"half");
        assert_eq!(
            read(&mut split.as_slice()),
            Err(Broken::Protocol),
            "a fragmented control frame was accepted"
        );
    }

    #[test]
    fn a_reserved_opcode_and_a_reserved_bit_are_both_refused() {
        let reserved_opcode = masked(true, 3, b"");
        assert_eq!(
            read(&mut reserved_opcode.as_slice()),
            Err(Broken::Protocol),
            "opcode 3 is reserved and was accepted"
        );
        let mut reserved_bit = masked(true, 1, b"hi");
        reserved_bit[0] |= 0b0100_0000;
        assert_eq!(
            read(&mut reserved_bit.as_slice()),
            Err(Broken::Protocol),
            "RSV1 was set by a peer that was told it has no extensions"
        );
    }

    #[test]
    fn an_empty_socket_ends_rather_than_fails() {
        let nothing: [u8; 0] = [];
        assert_eq!(
            read(&mut nothing.as_slice()),
            Err(Broken::Ended),
            "a client that went away should read as an end, not as a fault"
        );
    }

    #[test]
    fn a_server_frame_is_written_unmasked_in_the_shortest_form() {
        let mut out = Vec::new();
        write(&mut out, true, Opcode::Text, b"hello").expect("a writable buffer");
        assert_eq!(
            out,
            [0b1000_0001, 5, b'h', b'e', b'l', b'l', b'o'],
            "a server frame must carry no mask and must not use a longer length \
             form than it needs"
        );

        let mut medium = Vec::new();
        write(&mut medium, true, Opcode::Binary, &[0u8; 200]).expect("a writable buffer");
        assert_eq!(
            &medium[..4],
            &[0b1000_0010, 126, 0, 200],
            "a 200-byte payload takes the two-byte extended length"
        );
    }

    #[test]
    fn a_close_carries_its_code_and_an_empty_one_means_normal() {
        let mut out = Vec::new();
        close(&mut out, 1001).expect("a writable buffer");
        assert_eq!(out, [0b1000_1000, 2, 0x03, 0xe9], "1001 big-endian");
        assert_eq!(closed_with(&[0x03, 0xe9]), 1001, "the code is read back");
        assert_eq!(
            closed_with(&[]),
            1000,
            "a close with no payload is a normal close, not a malformed one"
        );
    }

    #[test]
    fn the_close_code_follows_from_why_reading_stopped() {
        assert_eq!(Broken::Protocol.code(), Some(1002));
        assert_eq!(Broken::TooLarge.code(), Some(1009));
        assert_eq!(
            Broken::Ended.code(),
            None,
            "there is nobody left to send a close to"
        );
    }
}
