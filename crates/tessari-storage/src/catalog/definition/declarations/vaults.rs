use super::*;

/// The key a vault's records are sealed under, sealed itself.
///
/// On the kind rather than beside it, for the reason [`EdgeDeclaration`] rides
/// on `Edge`: a field beside the kind would make "carries a key but is not a
/// vault" representable, and that state is a table full of records nothing can
/// ever open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VaultDeclaration {
    /// The vault's own key, and what it is sealed under.
    ///
    /// Every record in the vault has its data key wrapped under this one, so
    /// this single value is what stands between a stolen backend and every
    /// secret the vault holds — and it is itself unreadable without a
    /// passphrase that is never stored anywhere.
    pub custody: VaultCustody,
}

/// Who can open a vault's key (ADR-0093 D1), fixed when the vault is declared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VaultCustody {
    /// Sealed under the store's master key: the store's passphrase opens it,
    /// as it opens every other vault in this custody.
    Store(Wrapped),
    /// Sealed under a key derived from the vault's own passphrase, bound to the
    /// vault's scope. No master key is above it, so neither the store's
    /// passphrase nor store-wide authority opens it.
    Own(Root),
}

impl VaultCustody {
    /// The identifier of the vault key inside, whichever custody holds it.
    #[must_use]
    pub const fn key_id(&self) -> KeyId {
        match self {
            Self::Store(wrapped) => wrapped.key_id,
            Self::Own(root) => root.key_id,
        }
    }

    /// The word `INFO` answers with: `'store'` or `'own'`.
    #[must_use]
    pub const fn word(&self) -> &'static str {
        match self {
            Self::Store(_) => "store",
            Self::Own(_) => "own",
        }
    }
}

impl VaultDeclaration {
    /// The value written inside the table's catalog entry.
    ///
    /// Two opaque byte strings. Nothing here is a secret — the wrapped key is
    /// ciphertext under the store's master key, and the identifier is a random
    /// name rather than anything derived from key material — so the catalog can
    /// hold them the way it holds any other declaration.
    #[must_use]
    pub fn to_value(&self) -> Value {
        let key = match &self.custody {
            // A vault's own root is written exactly as the store's root is —
            // a salt beside the key id and the wrapped key — so the salt's
            // presence is what says which custody this is.
            VaultCustody::Own(root) => return VaultRoot(root.clone()).to_value(),
            VaultCustody::Store(key) => key,
        };
        Value::Object(BTreeMap::from([
            (
                FIELD_KEY_ID.to_owned(),
                Value::Bytes(key.key_id.bytes().to_vec()),
            ),
            (FIELD_WRAPPED.to_owned(), Value::Bytes(key.sealed.clone())),
        ]))
    }

    /// Read a declaration back.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CatalogMalformed`] when either part is missing or holds
    /// the wrong type. A vault whose key cannot be read is refused rather than
    /// treated as a vault with no key: the second reads as an empty store and
    /// would let a caller declare fields on it and write records that nothing
    /// could ever open.
    pub fn from_value(value: &Value) -> Result<Self> {
        const ENTITY: &str = "vault";
        let fields = object(value, ENTITY)?;
        if fields.contains_key(FIELD_SALT) {
            return Ok(Self {
                custody: VaultCustody::Own(VaultRoot::from_value(value)?.0),
            });
        }
        let Some(Value::Bytes(key_id)) = fields.get(FIELD_KEY_ID) else {
            return Err(Error::CatalogMalformed {
                entity: ENTITY,
                field: FIELD_KEY_ID,
                found: "missing or not bytes",
            });
        };
        let key_id = <[u8; KeyId::BYTES]>::try_from(key_id.as_slice()).map_err(|_| {
            Error::CatalogMalformed {
                entity: ENTITY,
                field: FIELD_KEY_ID,
                found: "the wrong number of bytes",
            }
        })?;
        let Some(Value::Bytes(sealed)) = fields.get(FIELD_WRAPPED) else {
            return Err(Error::CatalogMalformed {
                entity: ENTITY,
                field: FIELD_WRAPPED,
                found: "missing or not bytes",
            });
        };
        Ok(Self {
            custody: VaultCustody::Store(Wrapped {
                key_id: KeyId::adopt(key_id),
                sealed: sealed.clone(),
            }),
        })
    }
}
