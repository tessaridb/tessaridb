use super::*;

/// Try `attempt`, which checks a passphrase against `root`, under the bounds
/// sign-in has, counting misses in `store`'s table.
///
/// A passphrase is guessed exactly as a password is — an attempt costs the
/// guesser nothing and costs this node an Argon2id derivation — so it gets the
/// same two bounds, asked before the derivation runs: a run of misses is made
/// to wait, and only so many derivations run at once. The count is kept under
/// the root's salt, because what is being guessed is this store's passphrase
/// and not anybody's account: a guesser holding many accounts still gets three
/// tries, not three each. Unseal and change share it, so a change is not a
/// second, unthrottled way to test a guess.
pub(in crate::execute) fn guessed<T>(
    store: &tessari_storage::Store,
    root: &tessari_storage::VaultRoot,
    attempt: impl FnOnce() -> tessari_storage::Result<T>,
) -> Result<T> {
    let key = passphrase_key(root);
    if !store.attempts().permit(&key) {
        tracing::warn!("unseal refused: too many recent wrong passphrases");
        return Err(Error::PassphraseThrottled);
    }
    let Some(_verifying) = crate::throttle::verifying() else {
        tracing::warn!("unseal refused: already verifying as many as this node will");
        return Err(Error::PassphraseThrottled);
    };
    match attempt() {
        Ok(held) => {
            store.attempts().succeeded(&key);
            Ok(held)
        }
        Err(refused) => {
            if refused.is_wrong_key() {
                tracing::warn!("unseal refused: wrong passphrase");
                store.attempts().failed(&key);
            }
            Err(refused.into())
        }
    }
}
