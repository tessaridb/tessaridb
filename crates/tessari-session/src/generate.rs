//! Making an identifier that has never existed before.
//!
//! # Which of the store's two randomness doctrines this one takes
//!
//! There are two already, and they disagree on purpose. `storage::node` reads
//! the operating system's source directly and **refuses to open** when it
//! cannot, because *"the tempting fallback — a timestamp, a process id, a
//! counter — is what turns 'this cannot happen' into a collision"*.
//! `storage::transaction::commit` uses a per-thread xorshift instead, because
//! nothing in a backoff wait is a secret and the only property it needs is that
//! two writers do not compute the same number.
//!
//! A generated record identifier is the first kind and not the second. A
//! collision there is two records collapsing into one — silently, at write
//! time, with the loser simply overwritten — so this reads the same source
//! `node` does and refuses the same way.

use std::cell::RefCell;
use std::fs::File;
use std::io::Read;
use std::time::{SystemTime, UNIX_EPOCH};

use tessari_ql::{Function, Span};
use tessari_types::Value;

use crate::error::{Error, Result};

/// Where the bytes come from. No crate: sixteen bytes from a file is not worth a
/// dependency, and this workspace's dependency set is a measured budget.
const ENTROPY_SOURCE: &str = "/dev/urandom";

/// How many bytes a UUID is.
const UUID_LEN: usize = 16;

thread_local! {
    /// Opened once per thread and held open, rather than opened per call.
    ///
    /// A generated identifier is asked for **per record** by construction — a
    /// field's `DEFAULT` is evaluated for every write — so opening, reading and
    /// closing each time is three syscalls where one will do.
    ///
    /// **Deliberately not a `BufReader`.** Buffering would cut the syscalls by a
    /// factor of five hundred, and would hand a forked child the parent's
    /// unread bytes: the same identifier produced twice, in two processes, with
    /// nothing raising. That is exactly the collision this module exists to
    /// prevent, so the cheap five hundred is refused rather than taken.
    static SOURCE: RefCell<Option<File>> = const { RefCell::new(None) };
}

/// A fresh version-4 UUID, or a refusal.
///
/// # Why the version and variant bits are set
///
/// `Value::Uuid` is rendered by `uuid_to_text` in the canonical `8-4-4-4-12`
/// form, and `parse_uuid` accepts that form and nothing else — so a value of
/// this type is *claimed* to be a UUID everywhere it is written out, sent, or
/// read back. Sixteen bytes with whatever version nibble the kernel supplied is
/// not one, and a strict reader on the other side is right to say so. Six of the
/// 128 bits are spent saying what this is; the remaining 122 are what makes it
/// unique.
///
/// # Errors
///
/// Returns [`Error::CallFailed`] when the randomness source cannot be opened or
/// cannot be read. There is no fallback, for this module's reason.
pub(crate) fn uuid(span: Span) -> Result<Value> {
    let mut bytes = [0_u8; UUID_LEN];
    fill(&mut bytes).map_err(|_| Error::CallFailed {
        function: Function::RandUuid,
        reason: "the operating system's randomness source could not be read",
        span,
    })?;
    // Version 4 in the high nibble of the seventh byte, and the RFC's variant in
    // the top two bits of the ninth.
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Ok(Value::Uuid(bytes))
}

/// A fresh version-7 UUID's bytes, or a refusal.
///
/// # Why the store produces this one and `rand::uuid()` still produces version 4
///
/// A record identity is a **key**, and this store keeps its records in key
/// order. Version 4 is 122 bits of randomness, so consecutive inserts land at
/// unrelated points in the keyspace. Version 7 puts 48 bits of millisecond
/// timestamp in front, so records written together sort together — an ordering
/// property of the encoding, asserted by
/// `tests/inserts.rs::a_batch_written_later_sorts_after_one_written_before_it`.
///
/// What that ordering is worth at the storage layer has not been measured here,
/// so this comment does not say. Version 7 is the preference because the
/// ordering is useful on its own; any claim about what it costs or saves waits
/// on a benchmark rather than on an argument.
///
/// `rand::uuid()` is left alone at version 4 because a caller who wrote that
/// asked for it. This function has no caller in the language — it is what the
/// store reaches for when a caller supplied no identity at all — so nothing is
/// changed under anybody.
///
/// The bytes below the timestamp come from the same source and the same refusal
/// as [`uuid`]; this is that function with six of its random bytes replaced,
/// not a second doctrine about randomness.
///
/// # Errors
///
/// Returns [`Error::IdentityUnavailable`] when the randomness source cannot be
/// opened or read, or when the clock is set before 1970. There is no fallback,
/// for this module's reason.
pub(crate) fn uuid_v7(span: Span) -> Result<[u8; UUID_LEN]> {
    let mut bytes = [0_u8; UUID_LEN];
    fill(&mut bytes).map_err(|_| Error::IdentityUnavailable {
        reason: "the operating system's randomness source could not be read",
        span,
    })?;

    let since_epoch =
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| Error::IdentityUnavailable {
                reason: "the system clock reads a time before 1970",
                span,
            })?;
    // The low 48 bits of the millisecond count, taken as the low six bytes of
    // the big-endian `u64`. Destructured rather than shifted and masked: the
    // two leading bytes are dropped by the pattern, so there is no arithmetic
    // here to overflow and no index to get wrong. The count exceeds 48 bits in
    // the year 10889, at which point the leading bytes stop being zero and this
    // wraps — which is the format's own limit and not this function's.
    let millis = u64::try_from(since_epoch.as_millis()).unwrap_or(u64::MAX);
    let [_, _, t0, t1, t2, t3, t4, t5] = millis.to_be_bytes();
    bytes[0] = t0;
    bytes[1] = t1;
    bytes[2] = t2;
    bytes[3] = t3;
    bytes[4] = t4;
    bytes[5] = t5;

    // Version 7 in the high nibble of the seventh byte, and the RFC's variant in
    // the top two bits of the ninth — the same six bits [`uuid`] spends, for the
    // same reason: a value this store calls a UUID has to survive a strict
    // reader on the other side.
    bytes[6] = (bytes[6] & 0x0f) | 0x70;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Ok(bytes)
}

/// Fill the buffer from this thread's source, opening it on the first call.
fn fill(bytes: &mut [u8; UUID_LEN]) -> std::io::Result<()> {
    SOURCE.with_borrow_mut(|held| {
        if held.is_none() {
            *held = Some(File::open(ENTROPY_SOURCE)?);
        }
        let Some(source) = held.as_mut() else {
            // Unreachable — assigned on the line above. Said rather than
            // unwrapped, because "the branch above guarantees it" is a claim a
            // later edit can make false without touching this line.
            return Err(std::io::Error::other("no randomness source"));
        };
        // `read_exact` rather than `read`: a short read would hand out an
        // identifier whose tail is zeroes — precisely the predictable value this
        // refuses to produce — and it would do it without an error.
        let outcome = source.read_exact(bytes);
        if outcome.is_err() {
            // Drop the handle, so the next call opens a fresh descriptor rather
            // than reading again from one that has already failed.
            *held = None;
        }
        outcome
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic)]

    use tessari_ql::Span;
    use tessari_types::{Value, parse_uuid, uuid_to_text};

    use super::uuid;

    fn at() -> Span {
        Span::new(0, 1)
    }

    fn bytes() -> [u8; 16] {
        let Value::Uuid(held) = uuid(at()).expect("an identifier") else {
            panic!("a uuid");
        };
        held
    }

    #[test]
    fn two_identifiers_from_one_thread_differ() {
        // The check that stops every other assertion here from passing on a
        // constant — and the property the whole fold rewiring exists to keep.
        assert_ne!(bytes(), bytes());
    }

    #[test]
    fn an_identifier_is_not_all_zeroes() {
        // A short read that went unnoticed would look exactly like this.
        assert_ne!(bytes(), [0_u8; 16]);
    }

    #[test]
    fn what_is_generated_reads_back_as_a_uuid_and_not_merely_as_sixteen_bytes() {
        // The reason the version and variant bits are set at all: the canonical
        // text this value renders as is text another system parses, and a
        // strict reader refuses a version nibble the standard does not define.
        let held = bytes();
        let text = uuid_to_text(&held);
        assert_eq!(parse_uuid(&text), Some(held), "{text} did not read back");
        assert_eq!(held[6] >> 4, 4, "the version nibble is not 4: {text}");
        assert_eq!(held[8] >> 6, 0b10, "the variant bits are wrong: {text}");
    }

    #[test]
    fn setting_the_version_leaves_the_rest_of_the_bytes_alone() {
        // Two identifiers that agreed on more than the six fixed bits would mean
        // the masks had eaten entropy they were not meant to touch.
        let (first, second) = (bytes(), bytes());
        let shared = first
            .iter()
            .zip(second.iter())
            .filter(|(left, right)| left == right)
            .count();
        assert!(
            shared < 8,
            "{shared} of 16 bytes matched: {first:?} {second:?}"
        );
    }
}
