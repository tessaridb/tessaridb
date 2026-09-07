//! Sealing a vault record's secret fields, on the way to the payload encoder.
//!
//! # Why this runs here and not at the session layer
//!
//! The exfiltration survey measured the alternative. Decrypting and redacting at
//! the session layer leaves plaintext in the full-text index, in the change
//! feed, in the replication log and in every backup file, because all four
//! consume the **payload** and all four sit below the session. Sealing before
//! `encode_payload` closes them at once: the ciphertext *is* the stored value,
//! so a path that never learned what a vault is still cannot serve a secret,
//! because the plaintext is not in the store to be served.
//!
//! That is also why the marker lives on the field **definition** and not on the
//! statement: a write path that has to remember to seal is a write path that
//! will one day forget.
//!
//! # What a sealed record looks like
//!
//! Each secret field holds the sealed envelope of its own encoded value, and the
//! record gains one reserved entry holding the wrapped data key:
//!
//! ```text
//! { name: 'ada', secret_field: 0x…envelope…, "#keys": { "#vault": 0x…wrapped… } }
//! ```
//!
//! The key set is a **map keyed by recipient** rather than a single value,
//! because that is the shape sharing needs and the shape the design specifies:
//! adding a recipient is wrapping the same data key once more, which is a write
//! and never a decryption. Only `#vault` exists today.
//!
//! An entry is selected by its recipient name, deliberately, and never by the
//! key identifier in its envelope header — two wraps of one data key are two
//! independent envelopes with two identifiers, and matching on the identifier
//! would work for exactly as long as there was one recipient.

use std::collections::BTreeMap;

use tessari_types::{DatabaseId, NamespaceId, Value};
use tessari_vault::{Binding, Level, SecretBytes, Wrapped, keys};

use crate::catalog::{Catalog, TableDefinition};
use crate::error::{Error, Result};
use crate::store::Store;
use crate::transaction::{RecordAddress, Transaction};

/// The reserved entry holding a record's wrapped data keys.
///
/// Not a name the grammar can produce as a bare identifier — those are
/// `[A-Za-z_][A-Za-z0-9_]*` — but a **quoted** field name accepts any text, so
/// `{ "#keys": … }` does parse. The collision is therefore refused explicitly
/// rather than assumed away by the lexer, which is the difference between an
/// invariant and a coincidence that holds until someone adds quoting somewhere
/// else.
pub const KEYS_FIELD: &str = "#keys";

/// The recipient every record has: the vault itself.
pub const VAULT_RECIPIENT: &str = "#vault";

/// Seal every secret field of `payload`, if its table is a vault.
///
/// Returns the payload untouched for every table that is not a vault, and for a
/// vault with no secret fields declared — a vault is still a vault, and the
/// refusals it carries are not conditional on holding a secret today.
///
/// # Errors
///
/// - [`Error::Vault`] carrying `Sealed` when the store is sealed. Writing a
///   secret while sealed is refused rather than written in the clear, which is
///   the only safe direction: the alternative silently downgrades a vault to a
///   table whenever an operator forgets to unseal.
/// - [`Error::VaultReservedField`] when the payload carries the reserved key set
///   entry itself.
/// - [`Error::VaultNotAnObject`] when a vault is written a payload that is not
///   an object, since there are then no fields to seal and nothing that could
///   carry the key set.
pub fn seal_secrets(
    transaction: &mut Transaction<'_>,
    address: &RecordAddress,
    payload: Value,
) -> Result<Value> {
    let Some(definition) = Catalog::new(transaction).table(address.table)? else {
        return Ok(payload);
    };
    if !definition.is_vault() {
        return Ok(payload);
    }
    seal_into_vault(transaction, address, &definition, payload)
}

/// The vault half, once the table is known to be one.
fn seal_into_vault(
    transaction: &mut Transaction<'_>,
    address: &RecordAddress,
    definition: &TableDefinition,
    payload: Value,
) -> Result<Value> {
    let Value::Object(mut fields) = payload else {
        return Err(Error::VaultNotAnObject {
            table: definition.name.clone(),
        });
    };
    if fields.contains_key(KEYS_FIELD) {
        return Err(Error::VaultReservedField { field: KEYS_FIELD });
    }

    // Which declared fields are secret, intersected with what this write
    // actually carries. A declared secret field the write omits is simply
    // absent — sealing has nothing to do, and inventing a value here would
    // write a secret nobody supplied.
    let secrets: Vec<(String, tessari_types::FieldKind)> = Catalog::new(transaction)
        .fields_on(definition.id)?
        .into_iter()
        .filter(|field| field.secret && fields.contains_key(&field.name))
        .map(|field| (field.name, field.kind))
        .collect();
    if secrets.is_empty() {
        return Ok(Value::Object(fields));
    }

    // The declared type is checked HERE and nowhere else, because here is the
    // last moment the value exists as itself. The store's own schema check runs
    // at commit over the encoded payload, where a sealed field is bytes whatever
    // it was declared to hold — and a follower applying the same record holds
    // only ciphertext, so a check down there would be one the leader passes and
    // every replica fails. A declared type on a secret field would otherwise be
    // decoration, which is worse than not offering one.
    for (name, kind) in &secrets {
        let held = fields.get(name).unwrap_or(&Value::None);
        if !kind.accepts(held) {
            return Err(Error::SchemaViolation {
                table: Box::from(definition.name.as_str()),
                record: Box::from(address.id.to_string()),
                field: Box::from(name.as_str()),
                declared: Box::from(kind.name().as_ref()),
                // The **type** and never the value. Every other caller of this
                // variant renders what was found; this one cannot, because what
                // was found is the secret.
                found: Box::from(held.type_name()),
            });
        }
    }

    let Some(vault_key) = definition.vault_key() else {
        return Err(Error::VaultNoKey {
            table: definition.name.clone(),
        });
    };
    let table_scope = vault_scope(definition);
    let record_scope = record_scope(address);

    transaction.store().vault().with_master(|master| {
        let vault_key = keys::unwrap(master, Level::Vault, &table_scope, vault_key)?;
        let (wrapped, data_key) = keys::wrap_fresh(&vault_key, Level::Data, &record_scope)?;
        for (name, _) in &secrets {
            let plaintext = fields
                .get(name)
                .map(|value| tessari_encoding::encode_payload(value).into_bytes())
                .unwrap_or_default();
            let sealed = tessari_vault::envelope::seal(
                &data_key,
                wrapped.key_id,
                &field_binding(definition, &record_scope, name),
                &plaintext,
            )?;
            fields.insert(name.clone(), Value::Bytes(sealed));
        }
        fields.insert(KEYS_FIELD.to_owned(), key_set(&wrapped));
        Ok(())
    })?;

    Ok(Value::Object(fields))
}

/// The recipient-keyed set of wrapped data keys a record carries.
fn key_set(wrapped: &Wrapped) -> Value {
    Value::Object(
        [(
            VAULT_RECIPIENT.to_owned(),
            Value::Bytes(wrapped.sealed.clone()),
        )]
        .into_iter()
        .collect(),
    )
}

/// What binds a field's ciphertext to the one place it belongs.
fn field_binding<'a>(
    definition: &TableDefinition,
    record: &'a [u8],
    field: &'a str,
) -> Binding<'a> {
    Binding::Field {
        table: u64::from(definition.id.get()),
        record,
        field,
    }
}

/// What a vault's own key is bound to.
///
/// The **qualified name** rather than the table id, and not by preference: the
/// id is allocated inside `create_table`, so at the moment `DEFINE VAULT` has to
/// produce the wrapped key there is no id to bind it to. The name is unique
/// within its database — the catalog reserves it before the id is allocated —
/// which is the property the binding actually needs. Its job is to stop a
/// wrapped key being lifted out of one vault's declaration and pasted into
/// another's, and two vaults cannot share this string.
fn vault_scope(definition: &TableDefinition) -> Vec<u8> {
    vault_key_scope(definition.namespace, definition.database, &definition.name)
}

/// The same binding, for the statement that creates the key.
///
/// Public and taking the three parts separately because `DEFINE VAULT` has to
/// wrap the key before any definition exists to pass. **One function computes
/// this.** The alternative — the declaring statement building the scope one way
/// and the write path building it another — produces a vault that accepts every
/// write and opens nothing, with both sides looking correct in isolation. This
/// codebase has that scar already: an analyzer resolved in two places once
/// answered a correct query with nothing.
#[must_use]
pub fn vault_key_scope(namespace: NamespaceId, database: DatabaseId, name: &str) -> Vec<u8> {
    format!("{}:{}:{}", namespace.get(), database.get(), name).into_bytes()
}

/// What a record's data key is bound to.
///
/// The table id and the record's identity, in that order, with the id at a fixed
/// eight bytes so the two components cannot be read apart differently than they
/// were written. Here the id *is* available, and it is the stabler of the two
/// names — a vault's key is bound to something that exists before the table
/// does, and everything after that is bound to the table itself.
///
/// The identity is its **literal** spelling rather than the on-disk key
/// encoding, because the literal is the store's single canonical name for a
/// record — the protocol already promises a client can write back exactly what
/// it was given — and it keeps `1`, `'1'` and `uuid '…'` as three different
/// records, which they are. Without that, a wrapped key lifted from one record
/// into another would open.
fn record_scope(address: &RecordAddress) -> Vec<u8> {
    let mut scope = Vec::with_capacity(8 + 16);
    scope.extend_from_slice(&u64::from(address.table.get()).to_be_bytes());
    scope.extend_from_slice(address.id.to_literal().as_bytes());
    scope
}

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
    fields: &std::collections::BTreeMap<String, Value>,
) -> Result<SecretBytes> {
    let Some(vault_key) = definition.vault_key() else {
        return Err(Error::VaultNoKey {
            table: definition.name.clone(),
        });
    };
    let Some(Value::Object(recipients)) = fields.get(KEYS_FIELD) else {
        return Err(Error::VaultNoKey {
            table: definition.name.clone(),
        });
    };
    let Some(Value::Bytes(sealed)) = recipients.get(VAULT_RECIPIENT) else {
        return Err(Error::VaultNoKey {
            table: definition.name.clone(),
        });
    };

    let table_scope = vault_scope(definition);
    let record_scope = record_scope(address);
    transaction.store().vault().with_master(|master| {
        let vault_key = keys::unwrap(master, Level::Vault, &table_scope, vault_key)?;
        // The identifier in the header names the data key; `unwrap` proves it by
        // opening, so the copy here is only what the type needs.
        let wrapped = Wrapped {
            key_id: tessari_vault::envelope::key_id_of(sealed)?,
            sealed: sealed.clone(),
        };
        Ok(keys::unwrap(
            &vault_key,
            Level::Data,
            &record_scope,
            &wrapped,
        )?)
    })
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

/// Create the store's root record and unseal this process with it.
///
/// The two halves are one act deliberately. A root written without unsealing
/// leaves a store nobody can use until somebody re-presents a passphrase they
/// have just proven they know, and a process unsealed without a written root
/// holds a key that vanishes at restart with every secret sealed under it.
///
/// # Errors
///
/// Returns [`Error::Vault`] when the key derivation fails or when this process
/// is already unsealed — the second is the important one, because initialising
/// over a live keyring would strand every secret the running process can
/// currently open.
pub fn initialise_root(store: &Store, passphrase: &str) -> Result<crate::catalog::VaultRoot> {
    let (root, master) = tessari_vault::Root::create(passphrase)?;
    store.vault().adopt(master)?;
    Ok(crate::catalog::VaultRoot(root))
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
fn key_set_of<'a>(
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
