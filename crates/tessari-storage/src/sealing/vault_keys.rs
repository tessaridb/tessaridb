//! A vault's keys and recipients: minting, adding, removing and opening them.

use super::{
    KEYS_FIELD, VAULT_RECIPIENT, data_key_of, field_binding, record_scope, vault_key_scope,
};
use crate::catalog::TableDefinition;
use crate::error::{Error, Result};
use crate::store::Store;
use crate::transaction::{RecordAddress, Transaction};
use std::collections::BTreeMap;
use tessari_types::{DatabaseId, NamespaceId, Value};
use tessari_vault::{Level, SecretBytes, Wrapped, keys};

/// Open a record's data key, for the one statement allowed to want it.
///
/// Separate from the sealing above and deliberately not its inverse in shape:
/// sealing runs on every write to a vault, opening runs only where a caller
/// asked for it in so many words.
///
/// # Errors
///
/// Returns [`Error::Vault`] when the store is sealed or a key does not open, and
/// [`Error::VaultNoKey`] when the record carries no key set — which is what a
/// record written before its table became a vault looks like.
pub fn open_data_key(
    transaction: &Transaction<'_>,
    address: &RecordAddress,
    definition: &TableDefinition,
    fields: &BTreeMap<String, Value>,
) -> Result<SecretBytes> {
    let Some(keys_of_record) = fields.get(KEYS_FIELD) else {
        return Err(Error::VaultNoKey {
            table: definition.name.clone(),
        });
    };
    data_key_of(transaction, address, definition, keys_of_record).map(|(key, _)| key)
}

/// Open one sealed field, given the record's data key.
///
/// # Errors
///
/// Returns [`Error::Vault`] when the envelope does not open under this key and
/// this binding — which is what a ciphertext moved between records or between
/// fields looks like, and is the whole reason the binding exists.
pub fn open_field(
    data_key: &SecretBytes,
    address: &RecordAddress,
    definition: &TableDefinition,
    field: &str,
    sealed: &[u8],
) -> Result<Value> {
    let record_scope = record_scope(address);
    let plaintext = tessari_vault::envelope::open(
        data_key,
        &field_binding(definition, &record_scope, field),
        sealed,
    )?;
    Ok(tessari_encoding::decode_payload(&plaintext)?)
}

/// Mint a vault's own key, wrapped under the store's master key.
///
/// Here rather than in the session for the reason [`vault_key_scope`] is public:
/// the binding must be computed in one place, and the layer above has no
/// business holding an unwrapped key even for the length of a call. `DEFINE
/// VAULT` asks for a wrapped key and receives one.
///
/// # Errors
///
/// Returns [`Error::Vault`] carrying `Sealed` when the store is sealed. That is
/// what makes `DEFINE VAULT` the one declaration needing an unsealed store:
/// there is no way to defer the key without creating a vault that refuses every
/// write while reporting itself ready.
pub fn mint_vault_key(
    store: &Store,
    namespace: NamespaceId,
    database: DatabaseId,
    name: &str,
) -> Result<Wrapped> {
    let scope = vault_key_scope(namespace, database, name);
    store
        .vault()
        .with_master(|master| Ok(keys::wrap_fresh(master, Level::Vault, &scope)?.0))
}

/// Add a recipient to a record's key set.
///
/// # Why this takes a decoded payload and not a transaction
///
/// Because it must be provable at a glance that adding a recipient touches no
/// key, opens no envelope and needs no unsealed store. A function holding a
/// `Transaction` could reach the master key; this one cannot reach anything.
/// The property is structural rather than asserted, which matters most for the
/// removal below: revoking a recipient is the operation you least want to
/// depend on an operator being present to unseal.
///
/// The engine branches on exactly one name — [`VAULT_RECIPIENT`], its own — and
/// on nothing else about either half. The name is text it stores and returns,
/// the material is a value it stores and returns.
///
/// # Errors
///
/// - [`Error::VaultReservedRecipient`] when the name is the store's own entry.
/// - [`Error::VaultRecipientExists`] when the name is already on the record.
/// - [`Error::VaultNoKey`] when the record carries no key set at all, which is
///   what a record written before its table became a vault looks like.
pub fn add_recipient(
    fields: &mut BTreeMap<String, Value>,
    table: &str,
    recipient: &str,
    material: Value,
) -> Result<()> {
    let entries = key_set_of(fields, table)?;
    if recipient == VAULT_RECIPIENT {
        return Err(Error::VaultReservedRecipient {
            recipient: recipient.to_owned(),
        });
    }
    if entries.contains_key(recipient) {
        return Err(Error::VaultRecipientExists {
            recipient: recipient.to_owned(),
        });
    }
    entries.insert(recipient.to_owned(), material);
    Ok(())
}

/// Remove a recipient from a record's key set.
///
/// # Errors
///
/// - [`Error::VaultReservedRecipient`] when the name is the store's own entry —
///   removing it would leave a record nothing can ever open, which is a
///   crypto-shred and has its own statement.
/// - [`Error::VaultNoRecipient`] when no recipient of that name is there.
/// - [`Error::VaultNoKey`] when the record carries no key set at all.
pub fn remove_recipient(
    fields: &mut BTreeMap<String, Value>,
    table: &str,
    recipient: &str,
) -> Result<()> {
    let entries = key_set_of(fields, table)?;
    if recipient == VAULT_RECIPIENT {
        return Err(Error::VaultReservedRecipient {
            recipient: recipient.to_owned(),
        });
    }
    if entries.remove(recipient).is_none() {
        return Err(Error::VaultNoRecipient {
            recipient: recipient.to_owned(),
        });
    }
    Ok(())
}

/// The recipients of a record, without the store's own entry.
///
/// `#vault` is excluded and that is a decision rather than tidiness: it is not a
/// recipient anybody added, and putting it in a list beside the ones that can be
/// removed invites an attempt to remove the one entry that must never go.
///
/// # Errors
///
/// Returns [`Error::VaultNoKey`] when the record carries no key set.
pub fn recipients(
    fields: &BTreeMap<String, Value>,
    table: &str,
) -> Result<BTreeMap<String, Value>> {
    let Some(Value::Object(entries)) = fields.get(KEYS_FIELD) else {
        return Err(Error::VaultNoKey {
            table: table.to_owned(),
        });
    };
    Ok(entries
        .iter()
        .filter(|(name, _)| name.as_str() != VAULT_RECIPIENT)
        .map(|(name, material)| (name.clone(), material.clone()))
        .collect())
}

/// The mutable key set of a record, refusing a record that carries none.
pub(crate) fn key_set_of<'a>(
    fields: &'a mut BTreeMap<String, Value>,
    table: &str,
) -> Result<&'a mut BTreeMap<String, Value>> {
    match fields.get_mut(KEYS_FIELD) {
        Some(Value::Object(entries)) => Ok(entries),
        _ => Err(Error::VaultNoKey {
            table: table.to_owned(),
        }),
    }
}
