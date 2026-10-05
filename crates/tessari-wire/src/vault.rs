//! The vault frame's body (ADR-0092 D2).
//!
//! ```text
//! u8      credentials flag: 0 = none, 1 = present
//!         if 1: text user name, text password
//! u8      target: 0 = the store, 1 = one vault (ADR-0093 D6)
//!         if 1: text namespace, text database, text vault
//! u8      act: 1 status, 2 unseal, 3 seal, 4 change passphrase
//!         if 2: text passphrase
//!         if 4: text current passphrase, text new passphrase
//! ```
//!
//! The passphrase is a field of the frame and never a script: text in a script
//! is what a console keeps, a client logs on failure and a proxy records. The
//! node answers with an `Answer` frame of one value — the seal status — or a
//! `Refusal`.

use crate::error::{Error, Result};
use crate::frame::{put_text, take_text};

/// One act on the vault.
#[derive(Clone, PartialEq, Eq)]
pub enum VaultCall {
    /// Whether the node can open secrets, and until when.
    Status,
    /// Present the passphrase.
    Unseal(String),
    /// Drop the master key.
    Seal,
    /// Wrap the master key under a new passphrase (ADR-0092 D3).
    Change {
        /// The passphrase that opens the store now.
        current: String,
        /// The passphrase that will open it afterwards.
        new: String,
    },
}

/// A node wipes a passphrase when the call is dropped (G061, R-06).
#[cfg(feature = "server")]
impl Drop for VaultCall {
    fn drop(&mut self) {
        match self {
            Self::Unseal(passphrase) => zeroize::Zeroize::zeroize(passphrase),
            Self::Change { current, new } => {
                zeroize::Zeroize::zeroize(current);
                zeroize::Zeroize::zeroize(new);
            }
            Self::Status | Self::Seal => {}
        }
    }
}

/// Written by hand, because the derived one would print the passphrase.
impl std::fmt::Debug for VaultCall {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Status => "Status",
            Self::Unseal(_) => "Unseal(..)",
            Self::Seal => "Seal",
            Self::Change { .. } => "Change(..)",
        })
    }
}

/// One vault, by its tenancy and name — the target of an act on a vault that
/// carries its own passphrase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VaultPlace {
    /// The namespace it lives in.
    pub namespace: String,
    /// The database it lives in.
    pub database: String,
    /// Its name.
    pub vault: String,
}

/// A vault frame: the act, what it is about, and who is asking.
#[derive(Clone, PartialEq, Eq)]
pub struct VaultAsk {
    /// The act.
    pub call: VaultCall,
    /// The credentials, when the caller has any.
    pub credentials: Option<(String, String)>,
    /// One vault, or `None` for the store's own key.
    pub place: Option<VaultPlace>,
}

/// A node wipes the password when the ask is dropped (G061, R-06).
#[cfg(feature = "server")]
impl Drop for VaultAsk {
    fn drop(&mut self) {
        if let Some((_, password)) = &mut self.credentials {
            zeroize::Zeroize::zeroize(password);
        }
    }
}

/// Written by hand for [`crate::Request`]'s reason: the name is shown, the
/// password and the passphrase are not.
impl std::fmt::Debug for VaultAsk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VaultAsk")
            .field("call", &self.call)
            .field("place", &self.place)
            .field(
                "as",
                &self
                    .credentials
                    .as_ref()
                    .map_or("nobody", |(name, _)| name.as_str()),
            )
            .finish()
    }
}

impl VaultAsk {
    /// The body of a vault frame.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut body = Vec::new();
        match &self.credentials {
            Some((name, password)) => {
                body.push(1);
                put_text(&mut body, name);
                put_text(&mut body, password);
            }
            None => body.push(0),
        }
        match &self.place {
            Some(place) => {
                body.push(1);
                put_text(&mut body, &place.namespace);
                put_text(&mut body, &place.database);
                put_text(&mut body, &place.vault);
            }
            None => body.push(0),
        }
        match &self.call {
            VaultCall::Status => body.push(1),
            VaultCall::Unseal(passphrase) => {
                body.push(2);
                put_text(&mut body, passphrase);
            }
            VaultCall::Seal => body.push(3),
            VaultCall::Change { current, new } => {
                body.push(4);
                put_text(&mut body, current);
                put_text(&mut body, new);
            }
        }
        body
    }

    /// Read one back.
    ///
    /// # Errors
    ///
    /// [`Error::Malformed`] when the body does not hold what it claims, or
    /// carries bytes after it.
    pub fn decode(body: &[u8]) -> Result<Self> {
        let flag = body.first().copied().ok_or(Error::Malformed)?;
        let (credentials, at) = match flag {
            0 => (None, 1),
            1 => {
                let (name, at) = take_text(body, 1)?;
                let (password, at) = take_text(body, at)?;
                (Some((name, password)), at)
            }
            _ => return Err(Error::Malformed),
        };
        let target = body.get(at).copied().ok_or(Error::Malformed)?;
        let at = at.saturating_add(1);
        let (place, at) = match target {
            0 => (None, at),
            1 => {
                let (namespace, at) = take_text(body, at)?;
                let (database, at) = take_text(body, at)?;
                let (vault, at) = take_text(body, at)?;
                (
                    Some(VaultPlace {
                        namespace,
                        database,
                        vault,
                    }),
                    at,
                )
            }
            _ => return Err(Error::Malformed),
        };
        let act = body.get(at).copied().ok_or(Error::Malformed)?;
        let at = at.saturating_add(1);
        let (call, at) = match act {
            1 => (VaultCall::Status, at),
            2 => {
                let (passphrase, at) = take_text(body, at)?;
                (VaultCall::Unseal(passphrase), at)
            }
            3 => (VaultCall::Seal, at),
            4 => {
                let (current, at) = take_text(body, at)?;
                let (new, at) = take_text(body, at)?;
                (VaultCall::Change { current, new }, at)
            }
            _ => return Err(Error::Malformed),
        };
        if at != body.len() {
            return Err(Error::Malformed);
        }
        Ok(Self {
            call,
            credentials,
            place,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{VaultAsk, VaultCall, VaultPlace};

    #[test]
    fn a_vault_target_sits_between_the_credentials_and_the_act() {
        let asked = VaultAsk {
            call: VaultCall::Seal,
            credentials: None,
            place: Some(VaultPlace {
                namespace: "a".to_owned(),
                database: "b".to_owned(),
                vault: "c".to_owned(),
            }),
        };
        assert_eq!(
            asked.encode(),
            [
                0, 1, 0, 0, 0, 1, b'a', 0, 0, 0, 1, b'b', 0, 0, 0, 1, b'c', 3
            ]
        );
        let store = VaultAsk {
            call: VaultCall::Seal,
            credentials: None,
            place: None,
        };
        assert_eq!(store.encode(), [0, 0, 3]);
        assert!(
            VaultAsk::decode(&[0, 2, 3]).is_err(),
            "an unknown target was read"
        );
    }

    #[test]
    fn every_call_round_trips_and_trailing_bytes_are_refused() {
        for call in [
            VaultCall::Status,
            VaultCall::Unseal("a passphrase".to_owned()),
            VaultCall::Seal,
            VaultCall::Change {
                current: "old".to_owned(),
                new: "new".to_owned(),
            },
        ] {
            let places = [
                None,
                Some(VaultPlace {
                    namespace: "shop".to_owned(),
                    database: "live".to_owned(),
                    vault: "team".to_owned(),
                }),
            ];
            for (credentials, place) in [None, Some(("ada".to_owned(), "pw".to_owned()))]
                .into_iter()
                .flat_map(|credentials| places.clone().map(|place| (credentials.clone(), place)))
            {
                let asked = VaultAsk {
                    call: call.clone(),
                    credentials,
                    place,
                };
                let body = asked.encode();
                assert_eq!(VaultAsk::decode(&body).expect("decodes"), asked);
                let mut longer = body.clone();
                longer.push(0);
                assert!(
                    VaultAsk::decode(&longer).is_err(),
                    "a trailing byte was read"
                );
            }
        }
        assert!(
            VaultAsk::decode(&[0, 0, 9]).is_err(),
            "an unknown act was read"
        );
    }
}
