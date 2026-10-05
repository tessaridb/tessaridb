//! Every decoder the peer link reaches.
#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| tessari_wire::fuzzing::peer(data));
