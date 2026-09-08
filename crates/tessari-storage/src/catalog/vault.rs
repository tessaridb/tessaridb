//! The store's vault root record, as the catalog holds it.
//!
//! A thin translation between [`tessari_vault::Root`] and a catalog [`Value`].
//! It exists rather than deriving serialisation onto the vault type because the
//! vault crate must not know what a catalog record looks like: it is the piece
//! that has to stay auditable in isolation, and a dependency on the storage
//! layer's encoding would be one more thing to read before believing it.
//!
//! **Nothing in this record is a secret.** The salt is public by design and the
//! wrapped master key is ciphertext under a key derived from a passphrase that
//! is never stored anywhere. That is what makes it safe to replicate, to back
//! up, and to hold in the same catalog as everything else — an attacker holding
//! every byte of it has an offline guessing problem against Argon2id at the
//! OWASP floor, and nothing more.

use std::collections::BTreeMap;

use tessari_types::Value;
use tessari_vault::{KeyId, Root, keys::SALT_BYTES};

use crate::error::{Error, Result};

use super::definition::object;

const FIELD_SALT: &str = "salt";
const FIELD_KEY_ID: &str = "key_id";
const FIELD_WRAPPED: &str = "wrapped";
const ENTITY: &str = "vault root";

/// The store's vault root record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VaultRoot(pub Root);

impl VaultRoot {
    /// The value written to the catalog.
    #[must_use]
    pub fn to_value(&self) -> Value {
        Value::Object(BTreeMap::from([
            (FIELD_SALT.to_owned(), Value::Bytes(self.0.salt.to_vec())),
            (
                FIELD_KEY_ID.to_owned(),
                Value::Bytes(self.0.key_id.bytes().to_vec()),
            ),
            (
                FIELD_WRAPPED.to_owned(),
                Value::Bytes(self.0.wrapped.clone()),
            ),
        ]))
    }

    /// Read the record back.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CatalogMalformed`] when a part is missing, is not
    /// bytes, or is the wrong length. Every failure here is refused rather than
    /// defaulted: a root record read as absent would let an operator initialise
    /// a second one over the top, and every secret behind the first would still
    /// be in the store, permanently unopenable, with nothing in an error state.
    pub fn from_value(value: &Value) -> Result<Self> {
        let fields = object(value, ENTITY)?;
        let salt = fixed::<SALT_BYTES>(fields.get(FIELD_SALT), FIELD_SALT)?;
        let key_id = fixed::<{ KeyId::BYTES }>(fields.get(FIELD_KEY_ID), FIELD_KEY_ID)?;
        let Some(Value::Bytes(wrapped)) = fields.get(FIELD_WRAPPED) else {
            return Err(Error::CatalogMalformed {
                entity: ENTITY,
                field: FIELD_WRAPPED,
                found: "missing or not bytes",
            });
        };
        Ok(Self(Root {
            salt,
            key_id: KeyId::adopt(key_id),
            wrapped: wrapped.clone(),
        }))
    }
}

/// Read a fixed-width byte string, refusing anything else.
fn fixed<const N: usize>(value: Option<&Value>, field: &'static str) -> Result<[u8; N]> {
    let Some(Value::Bytes(bytes)) = value else {
        return Err(Error::CatalogMalformed {
            entity: ENTITY,
            field,
            found: "missing or not bytes",
        });
    };
    <[u8; N]>::try_from(bytes.as_slice()).map_err(|_| Error::CatalogMalformed {
        entity: ENTITY,
        field,
        found: "the wrong number of bytes",
    })
}
