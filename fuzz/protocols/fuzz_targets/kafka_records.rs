//! Record batches as a producer sends them, decompressed under the ceiling.
#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| drop(tessari_kafka::records::decode_batches(data)));
