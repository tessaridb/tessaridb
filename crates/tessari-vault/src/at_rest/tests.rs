use std::io::{Read as _, Write as _};

use super::*;
use crate::secret::KEY_BYTES;

fn key(byte: u8) -> AtRestKey {
    AtRestKey::from_key(&SecretBytes::adopt([byte; KEY_BYTES])).expect("subkeys")
}

fn sealed(key: &AtRestKey, plain: &[u8]) -> Vec<u8> {
    let mut sealing = key.seal_into(Vec::new()).expect("a sealing writer");
    sealing.write_all(plain).expect("written");
    sealing.finish().expect("finished")
}

fn opened(key: Option<&AtRestKey>, bytes: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut out = Vec::new();
    reading(key, bytes)?.read_to_end(&mut out)?;
    Ok(out)
}

fn refusal(result: std::io::Result<Vec<u8>>) -> String {
    result.expect_err("refused").to_string()
}

/// Sizes around the chunk boundary, where a stream construction goes wrong.
fn lengths() -> [usize; 6] {
    [0, 1, 65_535, 65_536, 65_537, 3 * 65_536 + 7]
}

fn plain(length: usize) -> Vec<u8> {
    (0..length)
        .map(|at| u8::try_from(at % 251).expect("small"))
        .collect()
}

#[test]
fn a_sealed_backup_opens_to_what_was_written_and_hides_it() {
    let key = key(7);
    for length in lengths() {
        let plain = plain(length);
        let bytes = sealed(&key, &plain);
        assert!(bytes.starts_with(SEALED_MAGIC));
        if length >= 64 {
            assert!(
                !bytes.windows(64).any(|window| window == &plain[..64]),
                "plaintext visible in a sealed backup of {length} bytes"
            );
        }
        assert_eq!(
            opened(Some(&key), &bytes).expect("opens"),
            plain,
            "{length} bytes"
        );
    }
}

#[test]
fn two_backups_of_the_same_bytes_differ() {
    let key = key(7);
    assert_ne!(sealed(&key, b"same"), sealed(&key, b"same"));
}

#[test]
fn a_plain_backup_passes_through_with_or_without_a_key() {
    let plain = b"TESSARILOG and the rest of a backup".to_vec();
    assert_eq!(opened(None, &plain).expect("read"), plain);
    assert_eq!(opened(Some(&key(7)), &plain).expect("read"), plain);
    assert_eq!(opened(None, b"short").expect("read"), b"short");
}

#[test]
fn a_sealed_backup_without_a_key_is_refused_naming_the_key() {
    let bytes = sealed(&key(7), b"records");
    assert!(
        refusal(opened(None, &bytes)).contains("opens only on a node given its encryption key")
    );
}

#[test]
fn a_sealed_backup_under_another_key_is_refused() {
    let bytes = sealed(&key(7), b"records");
    assert!(refusal(opened(Some(&key(8)), &bytes)).contains("does not open under this key"));
}

#[test]
fn a_cut_reordered_extended_or_flipped_backup_is_refused() {
    let key = key(7);
    let bytes = sealed(&key, &plain(3 * 65_536 + 7));
    let head = 21;
    let chunk = 65_536 + 16;
    // Cut on a chunk boundary: what is left ends on a chunk not marked last.
    let cut = bytes[..head + 2 * chunk].to_vec();
    // Cut mid-chunk.
    let torn = bytes[..bytes.len() - 5].to_vec();
    // The first two chunks swapped.
    let mut swapped = bytes[..head].to_vec();
    swapped.extend_from_slice(&bytes[head + chunk..head + 2 * chunk]);
    swapped.extend_from_slice(&bytes[head..head + chunk]);
    swapped.extend_from_slice(&bytes[head + 2 * chunk..]);
    // Bytes after the last chunk.
    let mut extended = bytes.clone();
    extended.extend_from_slice(&[0; 40]);
    // One bit in the middle.
    let mut flipped = bytes.clone();
    flipped[head + chunk + 100] ^= 1;
    // One bit in the head.
    let mut headed = bytes;
    headed[15] ^= 1;
    for (name, altered) in [
        ("cut", cut),
        ("torn", torn),
        ("swapped", swapped),
        ("extended", extended),
        ("flipped", flipped),
        ("head", headed),
    ] {
        assert!(
            refusal(opened(Some(&key), &altered)).contains("does not open under this key"),
            "{name} was not refused"
        );
    }
}

#[test]
fn a_marker_opens_only_under_its_own_key() {
    let marker = key(7).marker().expect("a marker");
    key(7).opens(&marker).expect("its own key opens it");
    assert!(matches!(key(8).opens(&marker), Err(Error::WrongKey)));
    let mut altered = marker;
    altered[3] ^= 1;
    assert!(matches!(key(7).opens(&altered), Err(Error::WrongKey)));
    assert!(matches!(key(7).opens(b"short"), Err(Error::WrongKey)));
}

#[test]
fn the_subkeys_differ_from_each_other_and_from_the_key() {
    let key = key(7);
    let raw = [7_u8; KEY_BYTES];
    let engine = *key.engine().expose();
    let backups = *key.backups().expose();
    assert_ne!(engine, raw);
    assert_ne!(backups, raw);
    assert_ne!(engine, backups);
}

#[cfg(unix)]
#[test]
fn a_key_file_must_be_32_private_bytes() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::tempdir().expect("a folder");
    let path = dir.path().join("key");
    std::fs::write(&path, [9_u8; KEY_BYTES]).expect("written");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("private");
    let read = AtRestKey::read(&path).expect("a private 32-byte key reads");
    assert_eq!(read.engine().expose(), key(9).engine().expose());

    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("shared");
    let shared = AtRestKey::read(&path)
        .expect_err("readable by others")
        .to_string();
    assert!(shared.contains("may be read by others"), "{shared}");

    std::fs::write(&path, b"0123456789abcdef0123456789abcdef\n").expect("a hex-ish line");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("private");
    let long = AtRestKey::read(&path).expect_err("33 bytes").to_string();
    assert!(long.contains("holds 33 bytes"), "{long}");

    let missing = AtRestKey::read(&dir.path().join("nothing")).expect_err("absent");
    assert!(matches!(missing, Error::KeyFile { .. }));
}
