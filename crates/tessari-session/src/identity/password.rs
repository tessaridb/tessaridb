//! Password hashing: Argon2id at the parameters every node shares.

use crate::error::{Error, Result};
use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::{Algorithm, Argon2, Params, Version};
use tessari_constants::{PASSWORD_HASH_LANES, PASSWORD_HASH_MEMORY_KIB, PASSWORD_HASH_PASSES};
use tessari_ql::Span;

/// The hasher both sides of a credential use, at parameters this project chose.
///
/// # Why not `Argon2::default()`
///
/// It was, and the values it gave were the right ones. The rule it broke is not
/// about the value: **LR-DB-004** says a default is read from the release in use
/// and recorded with it, because a default nobody read is not evidence. Inherited
/// from the crate, a `cargo update` that moved `Default` would move this store's
/// password-hashing posture with nothing in the repository, the decision records
/// or the tests saying so — and nothing would break, because a PHC string
/// carries its own parameters and old hashes keep verifying at their old cost.
/// Silence is the whole failure. The three numbers live in `tessari-constants`
/// and their reading is ADR-0043.
///
/// # Why one constructor and not two call sites
///
/// Hashing and verifying must agree, and two literal parameter sets are two
/// things that can drift. They would drift *quietly*: a verification at the wrong
/// parameters still succeeds, because the stored PHC string says what to use, so
/// the only symptom would be that new hashes stopped matching the recorded
/// intent — which nothing is looking at.
///
/// Returns `None` only if the constants are not a valid parameter set, which is
/// a condition of this repository rather than of any input, and is why
/// `the_pinned_parameters_are_a_valid_set` exists to fail in CI instead of here.
pub(crate) fn hasher() -> Option<Argon2<'static>> {
    Params::new(
        PASSWORD_HASH_MEMORY_KIB,
        PASSWORD_HASH_PASSES,
        PASSWORD_HASH_LANES,
        None,
    )
    .ok()
    .map(|params| Argon2::new(Algorithm::Argon2id, Version::V0x13, params))
}

/// Hash a password for storage.
///
/// # Errors
///
/// Returns [`Error::PasswordUnusable`] when the hasher refuses the input, which
/// it does for a password long enough to be a denial of service by itself.
pub(crate) fn hash(password: &str, span: Span) -> Result<String> {
    // Refused here rather than at each caller, so a path added later cannot set
    // one by forgetting to ask. An empty password is not a weak credential; it
    // is an account anybody holding the name can be.
    if password.is_empty() {
        return Err(Error::PasswordEmpty { span });
    }
    let salt = SaltString::generate(&mut OsRng);
    hasher()
        .ok_or(Error::PasswordUnusable { span })?
        .hash_password(password.as_bytes(), &salt)
        .map(|hashed| hashed.to_string())
        .map_err(|_| Error::PasswordUnusable { span })
}

/// Whether this password produces that stored hash.
///
/// A stored hash that cannot be parsed answers **false** rather than raising:
/// the alternative is that corrupting one byte of a credential turns a refusal
/// into a five-hundred, which tells an attacker more than a refusal does.
/// A hasher this build cannot construct answers **false** for the same reason,
/// which is the safe direction: no password matches anything until the
/// parameters are a set again.
pub(crate) fn verifies(password: &str, stored: &str) -> bool {
    PasswordHash::new(stored).is_ok_and(|parsed| {
        hasher().is_some_and(|argon| argon.verify_password(password.as_bytes(), &parsed).is_ok())
    })
}
