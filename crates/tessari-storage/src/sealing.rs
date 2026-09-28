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

mod vault_keys;
use std::collections::BTreeMap;

use tessari_types::{DatabaseId, NamespaceId, Value};
use tessari_vault::{Binding, Level, SecretBytes, Wrapped, keys};

use crate::catalog::{Catalog, TableDefinition};
use crate::error::{Error, Result};
use crate::store::Store;
use crate::transaction::{RecordAddress, Transaction};
pub use vault_keys::{
    add_recipient, mint_vault_key, open_data_key, open_field, recipients, remove_recipient,
};

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

    declared_types_hold(&fields, &secrets, definition, address)?;

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
        seal_each(
            &mut fields,
            &secrets,
            definition,
            &record_scope,
            &data_key,
            wrapped.key_id,
        )?;
        fields.insert(KEYS_FIELD.to_owned(), key_set(&wrapped));
        Ok(())
    })?;

    Ok(Value::Object(fields))
}

/// Seal the fields a partial edit named, under the key the record already has.
///
/// # What this is for, and what it is not
///
/// [`seal_secrets`] writes a record whole: it mints a fresh data key and a fresh
/// key set, which is right for a create and for a replacement, and wrong for an
/// edit — a fresh data key invalidates every wrap made for a recipient, so
/// rotating one field with `seal_secrets` silently discards everybody the record
/// was shared with.
///
/// This is the edit's path. The data key is the record's own, opened from the
/// key set the caller read out of the store, and the key set is written back
/// unchanged — so every recipient wrap stays valid, because the key those wraps
/// wrap has not moved.
///
/// # Which fields are plaintext, and why nothing has to guess
///
/// `named` is the set the edit assigned. Everything else in `payload` is the
/// stored envelope, carried through untouched. That is the whole reason this can
/// exist without a format change: the sealing path never has to tell a sealed
/// value from a plaintext one — a question with no sound answer, since a
/// `TYPE bytes SECRET` field's plaintext is bytes — because the layer that knows
/// says so.
///
/// # The key set is an argument and not a field of `payload`, deliberately
///
/// A payload carrying [`KEYS_FIELD`] is refused here exactly as it is on the
/// whole-record path, because that refusal is the control that stops a caller
/// injecting a key map. The key set therefore arrives as its own argument, read
/// from the stored record — so there is no shape of this call in which caller
/// input can reach the key set, rather than a comment asking the next caller to
/// be careful.
///
/// # Errors
///
/// The errors of [`seal_secrets`], plus [`Error::VaultNoKey`] when `keys` is not
/// a key set this store wrote.
pub fn reseal_named(
    transaction: &mut Transaction<'_>,
    address: &RecordAddress,
    payload: Value,
    keys_of_record: &Value,
    named: &std::collections::BTreeSet<String>,
) -> Result<Value> {
    let Some(definition) = Catalog::new(transaction).table(address.table)? else {
        return Ok(payload);
    };
    if !definition.is_vault() {
        return Ok(payload);
    }
    let Value::Object(mut fields) = payload else {
        return Err(Error::VaultNotAnObject {
            table: definition.name.clone(),
        });
    };
    if fields.contains_key(KEYS_FIELD) {
        return Err(Error::VaultReservedField { field: KEYS_FIELD });
    }

    // Declared secret, present in the write, AND named by the edit. The third
    // term is what makes this partial: a secret field the edit left alone is
    // already an envelope in `fields`, and sealing it again would seal the
    // ciphertext.
    let secrets: Vec<(String, tessari_types::FieldKind)> = Catalog::new(transaction)
        .fields_on(definition.id)?
        .into_iter()
        .filter(|field| {
            field.secret && named.contains(&field.name) && fields.contains_key(&field.name)
        })
        .map(|field| (field.name, field.kind))
        .collect();

    if secrets.is_empty() {
        // Nothing to seal, but the key set still has to go back on — the record
        // is a vault record and a write that dropped its key set would leave
        // every sealed field it carries unopenable.
        fields.insert(KEYS_FIELD.to_owned(), keys_of_record.clone());
        return Ok(Value::Object(fields));
    }
    declared_types_hold(&fields, &secrets, &definition, address)?;

    let record_scope = record_scope(address);
    let (data_key, key_id) = data_key_of(transaction, address, &definition, keys_of_record)?;
    seal_each(
        &mut fields,
        &secrets,
        &definition,
        &record_scope,
        &data_key,
        key_id,
    )?;
    fields.insert(KEYS_FIELD.to_owned(), keys_of_record.clone());
    Ok(Value::Object(fields))
}

/// Replace each named field with its sealed envelope.
///
/// The nonce is drawn fresh per call by [`tessari_vault::envelope::seal`], which
/// is what makes reusing one data key across a record's edits safe: the bound
/// that matters is the number of nonces drawn under one key, and a record edited
/// even millions of times stays astronomically short of the birthday bound for a
/// ninety-six bit random nonce.
fn seal_each(
    fields: &mut BTreeMap<String, Value>,
    secrets: &[(String, tessari_types::FieldKind)],
    definition: &TableDefinition,
    record_scope: &[u8],
    data_key: &SecretBytes,
    key_id: tessari_vault::KeyId,
) -> Result<()> {
    for (name, _) in secrets {
        let plaintext = fields
            .get(name)
            .map(|value| tessari_encoding::encode_payload(value).into_bytes())
            .unwrap_or_default();
        let sealed = tessari_vault::envelope::seal(
            data_key,
            key_id,
            &field_binding(definition, record_scope, name),
            &plaintext,
        )?;
        fields.insert(name.clone(), Value::Bytes(sealed));
    }
    Ok(())
}

/// Check every secret field against the type it was declared to hold.
///
/// Checked HERE and nowhere else, because here is the last moment the value
/// exists as itself. The store's own schema check runs at commit over the
/// encoded payload, where a sealed field is bytes whatever it was declared to
/// hold — and a follower applying the same record holds only ciphertext, so a
/// check down there would be one the leader passes and every replica fails. A
/// declared type on a secret field would otherwise be decoration, which is worse
/// than not offering one.
fn declared_types_hold(
    fields: &BTreeMap<String, Value>,
    secrets: &[(String, tessari_types::FieldKind)],
    definition: &TableDefinition,
    address: &RecordAddress,
) -> Result<()> {
    for (name, kind) in secrets {
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
    Ok(())
}

/// Open a record's data key from a key set, answering the identifier with it.
///
/// The identifier is what [`seal_each`] needs and what [`open_data_key`] has no
/// use for, which is why this sits underneath both rather than beside them.
fn data_key_of(
    transaction: &Transaction<'_>,
    address: &RecordAddress,
    definition: &TableDefinition,
    keys_of_record: &Value,
) -> Result<(SecretBytes, tessari_vault::KeyId)> {
    let Some(vault_key) = definition.vault_key() else {
        return Err(Error::VaultNoKey {
            table: definition.name.clone(),
        });
    };
    let Value::Object(recipients) = keys_of_record else {
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
        let opened = keys::unwrap(&vault_key, Level::Data, &record_scope, &wrapped)?;
        Ok((opened, wrapped.key_id))
    })
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
