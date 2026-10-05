//! The log-backup and state-snapshot verifiers: a file handed to `--verify`.
#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    drop(tessari_backup::verify(&mut std::io::Cursor::new(data)));
    drop(tessari_backup::verify_state(&mut std::io::Cursor::new(data)));
});
