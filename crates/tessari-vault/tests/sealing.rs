//! The negative matrix for the sealed-value core.
//!
//! A working seal-and-open is not evidence that this crate is correct — it is
//! evidence that the happy path works, which is the one path an attacker will
//! not take. So the round trip is one test here and the other seventeen are
//! things that must **fail**: a wrong key, altered bytes, truncated bytes, a
//! rewritten header, a ciphertext lifted into another record, another field or
//! another level of the hierarchy, a swapped salt, and a second unseal.
//!
//! Two of them are not about cryptography at all and are here because they are
//! the two ways this code could be correct and still leak: a plaintext left in
//! the output buffer, and key material rendered by a `Debug` somebody derived.

use tessari_vault::envelope::{self, ALGORITHM_CHACHA20_POLY1305, HEADER_BYTES};
use tessari_vault::{Binding, Error, Keyring, Level, Root, SecretBytes, keys};

const TABLE: u64 = 7;
const PLAINTEXT: &[u8] = b"correct horse battery staple";

fn field(record: &'static [u8], name: &'static str) -> Binding<'static> {
    Binding::Field {
        table: TABLE,
        record,
        field: name,
    }
}

fn sealed_sample() -> (SecretBytes, Vec<u8>) {
    let key = SecretBytes::generate().expect("entropy");
    let key_id = tessari_vault::KeyId::generate().expect("entropy");
    let sealed = envelope::seal(&key, key_id, &field(b"github", "password"), PLAINTEXT)
        .expect("sealing a value must succeed");
    (key, sealed)
}

#[test]
fn a_sealed_value_opens_to_what_was_sealed() {
    let (key, sealed) = sealed_sample();
    let opened = envelope::open(&key, &field(b"github", "password"), &sealed).expect("opens");
    assert_eq!(opened, PLAINTEXT);
}

#[test]
fn the_plaintext_is_nowhere_in_the_sealed_bytes() {
    // The planted-plaintext scan, at the level this crate can assert it. The
    // same scan against the raw storage backend, a real backup artifact and a
    // replica is criterion K2 and belongs to a later wave — this one proves the
    // property holds before it is written anywhere.
    let (_key, sealed) = sealed_sample();
    assert!(
        !sealed.windows(PLAINTEXT.len()).any(|w| w == PLAINTEXT),
        "the plaintext appears verbatim in the sealed bytes"
    );
    for word in [&b"correct"[..], b"horse", b"battery", b"staple"] {
        assert!(
            !sealed.windows(word.len()).any(|w| w == word),
            "a fragment of the plaintext survives in the sealed bytes"
        );
    }
}

#[test]
fn sealing_the_same_value_twice_produces_different_bytes() {
    // A fresh nonce per call. Identical outputs would mean an attacker who sees
    // two records can tell that they hold the same secret without opening
    // either.
    let key = SecretBytes::generate().expect("entropy");
    let key_id = tessari_vault::KeyId::generate().expect("entropy");
    let first = envelope::seal(&key, key_id, &field(b"a", "password"), PLAINTEXT).expect("seals");
    let second = envelope::seal(&key, key_id, &field(b"a", "password"), PLAINTEXT).expect("seals");
    assert_ne!(first, second);
}

#[test]
fn a_wrong_key_does_not_open_it() {
    let (_key, sealed) = sealed_sample();
    let other = SecretBytes::generate().expect("entropy");
    assert_eq!(
        envelope::open(&other, &field(b"github", "password"), &sealed),
        Err(Error::WrongKey)
    );
}

#[test]
fn an_altered_ciphertext_does_not_open() {
    let (key, mut sealed) = sealed_sample();
    let last = sealed.len() - 1;
    sealed[last] ^= 0x01;
    assert_eq!(
        envelope::open(&key, &field(b"github", "password"), &sealed),
        Err(Error::WrongKey)
    );
}

#[test]
fn an_altered_key_id_does_not_open() {
    // The header is authenticated, so rewriting the identifier of the key that
    // opens a value turns into a refusal rather than into the wrong key being
    // trusted.
    let (key, mut sealed) = sealed_sample();
    sealed[3] ^= 0xff;
    assert_eq!(
        envelope::open(&key, &field(b"github", "password"), &sealed),
        Err(Error::WrongKey)
    );
}

#[test]
fn an_altered_nonce_does_not_open() {
    let (key, mut sealed) = sealed_sample();
    sealed[HEADER_BYTES - 1] ^= 0xff;
    assert_eq!(
        envelope::open(&key, &field(b"github", "password"), &sealed),
        Err(Error::WrongKey)
    );
}

#[test]
fn a_downgraded_algorithm_is_refused_by_name() {
    let (key, mut sealed) = sealed_sample();
    sealed[1] = ALGORITHM_CHACHA20_POLY1305 + 1;
    assert_eq!(
        envelope::open(&key, &field(b"github", "password"), &sealed),
        Err(Error::UnknownAlgorithm(ALGORITHM_CHACHA20_POLY1305 + 1))
    );
}

#[test]
fn a_later_format_version_is_refused_rather_than_guessed_at() {
    let (key, mut sealed) = sealed_sample();
    sealed[0] = 2;
    assert_eq!(
        envelope::open(&key, &field(b"github", "password"), &sealed),
        Err(Error::UnknownVersion(2))
    );
}

#[test]
fn truncated_bytes_are_not_a_sealed_value() {
    let (key, sealed) = sealed_sample();
    for length in [0, 1, HEADER_BYTES, HEADER_BYTES + 15] {
        assert_eq!(
            envelope::open(&key, &field(b"github", "password"), &sealed[..length]),
            Err(Error::NotSealed),
            "{length} bytes was accepted as a sealed value"
        );
    }
}

#[test]
fn a_value_moved_to_another_record_does_not_open() {
    // Encryption says the bytes are secret; it says nothing about where they
    // belong. Without the binding this passes, and the store has been lied to
    // in its own bytes with every checksum intact.
    let (key, sealed) = sealed_sample();
    assert_eq!(
        envelope::open(&key, &field(b"gitlab", "password"), &sealed),
        Err(Error::WrongKey)
    );
}

#[test]
fn a_value_moved_to_another_field_does_not_open() {
    let (key, sealed) = sealed_sample();
    assert_eq!(
        envelope::open(&key, &field(b"github", "recovery_code"), &sealed),
        Err(Error::WrongKey)
    );
}

#[test]
fn a_value_moved_to_another_table_does_not_open() {
    let (key, sealed) = sealed_sample();
    let elsewhere = Binding::Field {
        table: TABLE + 1,
        record: b"github",
        field: "password",
    };
    assert_eq!(
        envelope::open(&key, &elsewhere, &sealed),
        Err(Error::WrongKey)
    );
}

#[test]
fn the_binding_cannot_be_confused_by_where_one_component_ends() {
    // Every component is length-prefixed. Without that, ("ab", "c") and
    // ("a", "bc") hash to the same bytes and two different places share one
    // binding — the property being bought, lost to a detail.
    let key = SecretBytes::generate().expect("entropy");
    let key_id = tessari_vault::KeyId::generate().expect("entropy");
    let sealed = envelope::seal(&key, key_id, &field(b"ab", "c"), PLAINTEXT).expect("seals");
    assert_eq!(
        envelope::open(&key, &field(b"a", "bc"), &sealed),
        Err(Error::WrongKey)
    );
}

#[test]
fn a_wrapped_key_cannot_be_presented_at_another_level() {
    // All four levels are thirty-two random bytes and are otherwise
    // indistinguishable. An attacker who can move one where another is expected
    // rearranges the hierarchy without breaking a single tag.
    let master = SecretBytes::generate().expect("entropy");
    let (wrapped, _vault_key) = keys::wrap_fresh(&master, Level::Vault, b"team").expect("wraps");
    assert!(keys::unwrap(&master, Level::Vault, b"team", &wrapped).is_ok());
    assert_eq!(
        keys::unwrap(&master, Level::Data, b"team", &wrapped).err(),
        Some(Error::WrongKey)
    );
    assert_eq!(
        keys::unwrap(&master, Level::Vault, b"other", &wrapped).err(),
        Some(Error::WrongKey)
    );
}

#[test]
fn a_wrapped_key_unwraps_to_the_same_key() {
    let master = SecretBytes::generate().expect("entropy");
    let (wrapped, key) = keys::wrap_fresh(&master, Level::Data, b"team:github").expect("wraps");
    let recovered = keys::unwrap(&master, Level::Data, b"team:github", &wrapped).expect("unwraps");
    assert_eq!(key.expose(), recovered.expose());
}

#[test]
fn a_second_recipient_opens_the_same_record_without_anything_being_decrypted() {
    // The property the whole four-level hierarchy exists to buy: adding a
    // recipient is a write. The data key is sealed again under another party's
    // key, the record's ciphertext is untouched, and no plaintext is in hand.
    let vault_key = SecretBytes::generate().expect("entropy");
    let (wrapped_for_vault, data_key) =
        keys::wrap_fresh(&vault_key, Level::Data, b"team:github").expect("wraps");
    let sealed = envelope::seal(
        &data_key,
        wrapped_for_vault.key_id,
        &field(b"github", "password"),
        PLAINTEXT,
    )
    .expect("seals");

    let recipient = SecretBytes::generate().expect("entropy");
    let wrapped_for_recipient =
        keys::wrap(&recipient, Level::Data, b"team:github", &data_key).expect("wraps");

    let theirs = keys::unwrap(
        &recipient,
        Level::Data,
        b"team:github",
        &wrapped_for_recipient,
    )
    .expect("unwraps");
    assert_eq!(
        envelope::open(&theirs, &field(b"github", "password"), &sealed).expect("opens"),
        PLAINTEXT
    );
}

#[test]
fn a_root_record_unlocks_with_its_passphrase_and_not_another() {
    let (root, master) = Root::create("a passphrase").expect("creates");
    let recovered = root.unlock("a passphrase").expect("unlocks");
    assert_eq!(master.expose(), recovered.expose());
    assert_eq!(
        root.unlock("another passphrase").err(),
        Some(Error::WrongKey)
    );
}

#[test]
fn a_root_record_with_a_swapped_salt_does_not_unlock() {
    // The salt is authenticated with the wrapped master key, so a root record
    // cannot be assembled from parts of two others.
    let (mut root, _master) = Root::create("a passphrase").expect("creates");
    root.salt[0] ^= 0xff;
    assert_eq!(root.unlock("a passphrase").err(), Some(Error::WrongKey));
}

#[test]
fn a_root_record_holds_no_key() {
    // What an attacker gets from the persisted record: a salt, an identifier,
    // and a wrapped key. Not the master key itself.
    let (root, master) = Root::create("a passphrase").expect("creates");
    let exposed = master.expose();
    assert!(
        !root.wrapped.windows(exposed.len()).any(|w| w == exposed),
        "the master key appears verbatim in the record that is written to disk"
    );
}

#[test]
fn a_keyring_starts_sealed_and_returns_there() {
    let (root, _master) = Root::create("a passphrase").expect("creates");
    let mut keyring = Keyring::sealed();

    assert!(keyring.is_sealed());
    assert_eq!(keyring.master().err(), Some(Error::Sealed));

    keyring.unseal(&root, "a passphrase").expect("unseals");
    assert!(!keyring.is_sealed());
    assert!(keyring.master().is_ok());

    keyring.seal();
    assert!(keyring.is_sealed());
    assert_eq!(keyring.master().err(), Some(Error::Sealed));
}

#[test]
fn a_wrong_passphrase_leaves_the_keyring_sealed() {
    let (root, _master) = Root::create("a passphrase").expect("creates");
    let mut keyring = Keyring::sealed();
    assert_eq!(keyring.unseal(&root, "not it").err(), Some(Error::WrongKey));
    assert!(keyring.is_sealed());
}

#[test]
fn a_second_unseal_is_refused_rather_than_replacing_the_key() {
    // Silently replacing it would change which values open, with nothing
    // recording that anything happened.
    let (root, _master) = Root::create("a passphrase").expect("creates");
    let mut keyring = Keyring::sealed();
    keyring.unseal(&root, "a passphrase").expect("unseals");
    assert_eq!(
        keyring.unseal(&root, "a passphrase").err(),
        Some(Error::AlreadyUnsealed)
    );
}

#[test]
fn key_material_does_not_render() {
    // The one route key material takes to a log is that somebody formatted a
    // struct containing it, and the containing struct is usually derived
    // `Debug` by a person who never thought about the field. Making the leaf
    // unprintable makes every container safe without anyone having to notice.
    let key = SecretBytes::generate().expect("entropy");
    let rendered = format!("{key:?}");
    // Exact equality rather than a "does not contain the bytes" scan: the scan
    // would pass for any rendering that happened to avoid this key's bytes,
    // including a length or a fingerprint, and both of those are leaks of a
    // smaller size rather than of none.
    assert_eq!(rendered, "SecretBytes(<redacted>)");

    // And a struct that holds one renders no better, which is the property that
    // actually matters.
    #[derive(Debug)]
    #[allow(dead_code)]
    struct Holder {
        name: &'static str,
        key: SecretBytes,
    }
    let held = format!(
        "{:?}",
        Holder {
            name: "master",
            key: SecretBytes::generate().expect("entropy"),
        }
    );
    assert!(held.contains("<redacted>"));
}
