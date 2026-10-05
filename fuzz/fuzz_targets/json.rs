//! The one JSON reader: HTTP envelopes, `json::parse`, ingestion.
#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| drop(tessari_types::json::read(data)));
