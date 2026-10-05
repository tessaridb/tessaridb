//! Entry points for the fuzz targets, and nothing else (G061 C3).
//!
//! Every decoder a node runs on bytes from a socket is reachable from here, so
//! the fuzz crate can drive them without the decoders becoming public API. The
//! module exists only under the `fuzzing` feature, which nothing but the fuzz
//! crate turns on. Each function takes arbitrary bytes and must return —
//! whatever it is given, a decoder answers with a value or a refusal, and a
//! panic, a hang or an allocation the input did not pay for is the finding.

use std::io::Cursor;

/// The frame header and every decoder a client connection reaches.
///
/// The first byte chooses the decoder; the rest is its body. The whole input is
/// also read as a stream of frames, which is where the length prefix and the
/// ceiling are judged.
pub fn client(data: &[u8]) {
    drop(crate::frame::read_tagged(&mut Cursor::new(data)));
    let Some((&choice, body)) = data.split_first() else {
        return;
    };
    match choice % 6 {
        0 => drop(crate::message::Request::decode(body)),
        1 => drop(crate::push::Follow::decode(body)),
        2 => drop(crate::push::Happened::decode(body)),
        3 => drop(crate::vault::VaultAsk::decode(body)),
        4 => drop(crate::redirect::Elsewhere::decode(body)),
        _ => drop(crate::message::decode_outcome(body, 0)),
    }
}

/// Every decoder the peer link reaches, behind its mutual TLS.
pub fn peer(data: &[u8]) {
    let Some((&choice, body)) = data.split_first() else {
        return;
    };
    match choice % 15 {
        0 => drop(crate::peer::Hello::decode(body)),
        1 => drop(crate::grant::Ballot::decode(body)),
        2 => drop(crate::grant::Vote::decode(body)),
        3 => drop(crate::collection::Collect::decode(body)),
        4 => drop(crate::collection::Collected::decode(body)),
        5 => drop(crate::collection::StreamAsk::decode(body)),
        6 => drop(crate::collection::Streamed::decode(body)),
        7 => drop(crate::gathering::Gather::decode(body)),
        8 => drop(crate::gathering::Page::decode(body)),
        9 => drop(crate::coordination::Coordinate::decode(body)),
        10 => drop(crate::coordination::decode_answer(body)),
        11 => drop(crate::across::Carried::decode(body)),
        12 => drop(crate::budget::Attempt::decode(body)),
        13 => drop(crate::assertion::Signed::decode(body, 0)),
        _ => drop(crate::frame::read_tagged(&mut Cursor::new(body))),
    }
}
