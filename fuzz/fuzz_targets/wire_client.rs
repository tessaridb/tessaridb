//! The frame header and every decoder a client connection reaches.
#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| tessari_wire::fuzzing::client(data));
