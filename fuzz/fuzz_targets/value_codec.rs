//! The store's value codec, which the wire and the backups both carry.
#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| drop(tessari_encoding::decode_payload(data)));
