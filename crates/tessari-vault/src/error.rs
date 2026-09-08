//! What can go wrong opening or sealing a value.
//!
//! # Not one of these carries a value
//!
//! Every variant here names a *kind* of failure and never the data that
//! failed. That is a rule with a specific target: this store's refusal
//! formatter has one variant that quotes the offending value rather than its
//! type, which is helpful everywhere in the language and is a leak on this one
//! surface. An error enum whose variants cannot hold a value cannot participate
//! in that leak even if a later caller wires it in carelessly.
//!
//! # `WrongKey` is deliberately one variant
//!
//! A wrong key, a tampered ciphertext, a truncated ciphertext and a ciphertext
//! moved to another field all fail authentication, and all report the same
//! thing. Splitting them would tell a caller which of its guesses was closer,
//! which is an oracle. The distinctions exist in the tests, where the caller is
//! the author.

/// The result of a vault operation.
pub type Result<T> = core::result::Result<T, Error>;

/// A vault operation that did not succeed.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Error {
    /// The operating system refused to supply entropy.
    ///
    /// Carrying on would mean generating a key from whatever a buffer held.
    #[error("the system's entropy source is unavailable")]
    Entropy,

    /// The bytes are not a sealed value.
    ///
    /// Too short to hold a header, or a header that does not parse. Distinct
    /// from [`Error::WrongKey`] because nothing was authenticated: this is a
    /// framing failure, and reporting it as a key failure would send an
    /// operator looking for the wrong problem.
    #[error("these bytes are not a sealed value")]
    NotSealed,

    /// A sealed value written by a build that used a format this one does not
    /// know.
    ///
    /// Refused rather than guessed at. A value written under a later layout and
    /// read as though it used this one would decrypt to plausible rubbish or
    /// authenticate against the wrong associated data.
    #[error("this sealed value uses format version {0}, which this build does not know")]
    UnknownVersion(u8),

    /// A sealed value encrypted under an algorithm this build does not carry.
    #[error("this sealed value uses algorithm {0}, which this build does not carry")]
    UnknownAlgorithm(u8),

    /// The value did not authenticate.
    ///
    /// The wrong key, altered bytes, truncated bytes, or a ciphertext lifted
    /// from another field or another record — one answer for all of them, on
    /// purpose. See the module documentation.
    #[error(
        "this value did not open: the key is wrong, or the bytes are not the ones that were sealed"
    )]
    WrongKey,

    /// A passphrase could not be turned into a key.
    ///
    /// The key-derivation function refused — a malformed salt, or parameters
    /// the implementation rejects.
    #[error("the passphrase could not be turned into a key")]
    Derivation,

    /// The store is sealed, so nothing can be opened.
    ///
    /// This is the *open* refusal and never the *reach* refusal. A caller who
    /// may not address the vault at all is turned away before reaching here, by
    /// a different mechanism, so that neither refusal can be satisfied by the
    /// other's route.
    #[error("the vault is sealed")]
    Sealed,

    /// The store is already unsealed.
    ///
    /// Refused rather than silently re-derived, because a second unseal with a
    /// different passphrase would otherwise replace the master key held in
    /// memory and quietly change which values open.
    #[error("the vault is already unsealed")]
    AlreadyUnsealed,
}
