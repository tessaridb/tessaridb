//! RESP commands, array and inline, arriving in pieces as a socket hands them
//! over: the first byte says how long each piece is.
#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    let Some((&piece, rest)) = data.split_first() else {
        return;
    };
    let piece = usize::from(piece).max(1);
    let mut decoder = tessari_resp::codec::Decoder::new();
    for chunk in rest.chunks(piece) {
        decoder.extend(chunk);
        while let Ok(Some(_)) = decoder.command() {}
    }
});
