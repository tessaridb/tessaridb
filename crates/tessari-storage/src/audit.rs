//! The audit trail for reads of a vault.
//!
//! # The ordering is the whole mechanism
//!
//! The record is written and committed **before** any plaintext leaves the
//! process, and a trail that cannot be written **refuses the read**. Both halves
//! are load-bearing and neither is visible in normal operation: written
//! afterwards, every crash, kill, timeout and partial write produces a secret
//! release with no record, and an attacker who can induce any of those reads
//! without a trace. The two orderings are indistinguishable when nothing fails,
//! which is why the defect survives every test that does not look for it.
//!
//! Refusing on failure is what makes accountability a property rather than an
//! aspiration. A store that serves when it cannot record is a store whose audit
//! trail an attacker disables first. The cost is real and is stated rather than
//! discovered: a broken trail is an outage of every `REVEAL`.
//!
//! # The quorum is all-must-succeed, and that is a decision
//!
//! Any installed device failing refuses the read. The alternative —
//! any-must-succeed — trades accountability for availability and is the right
//! answer for some deployments, but it has to be chosen rather than inherited,
//! so it is not offered here until somebody asks for it.
//!
//! # What the built-in device does not give you
//!
//! It writes into this store, so it shares a failure domain and an access path
//! with the thing it audits. That is the arrangement an independent audit
//! pipeline exists to avoid, and an embedded engine cannot offer one: whoever
//! holds this store's backend already holds everything. The gap is published
//! rather than papered over, and [`AuditDevice`] is the seam a deployment
//! installs an independent device into.
//!
//! There is no hash chain either. It was considered and left out of the minimum
//! program for a reason worth writing down: chaining serialises every read
//! through one key, turning concurrent reveals into a queue and a conflict, and
//! what it buys is tamper-evidence against an attacker who holds the backend —
//! the attacker the vault's own out-of-scope list already names. An independent
//! device is the answer to that threat, not a chain written by the party under
//! suspicion.
//!
//! # Identity, never value
//!
//! The record carries who, what record, which field names, and whether it was
//! served. It never carries a secret, a fragment of one, or a length that
//! discloses one. The log pipeline is the widest-read and longest-retained
//! system in any architecture, and putting values in it makes the least
//! protected system the most valuable one, permanently.

use std::sync::{Arc, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use tessari_types::{DatabaseId, Datetime, NamespaceId, RecordId, Value};

use crate::catalog::system;
use crate::error::{Error, Result};
use crate::store::Store;

/// One read of a vault, as the trail records it.
///
/// Borrowed throughout: an event is built at the moment of the read and consumed
/// before the statement returns, so nothing here needs to own anything, and a
/// device that wants to keep a copy has to say so.
#[derive(Debug, Clone, Copy)]
pub struct VaultRead<'a> {
    /// Who asked. The signed-in user's name, or `anonymous`.
    pub actor: &'a str,
    /// The namespace the vault lives in.
    pub namespace: NamespaceId,
    /// The database the vault lives in.
    pub database: DatabaseId,
    /// The vault's name.
    pub vault: &'a str,
    /// The record, spelled as the language spells an identity.
    pub record: &'a str,
    /// The field **names** that were asked for. Never their contents.
    pub fields: &'a [String],
    /// Whether the read was served or refused.
    pub served: bool,
}

/// Somewhere a read is recorded.
///
/// Implemented by a deployment that wants the trail on a failure domain this
/// store does not control — which is the arrangement the module documentation
/// recommends and cannot provide.
///
/// # Errors
///
/// An implementation returns an error when it could not durably record the
/// event. That refuses the read, so an implementation that cannot distinguish
/// "written" from "queued" should not report success.
pub trait AuditDevice: Send + Sync + std::fmt::Debug {
    /// Record one read, durably, before the caller returns.
    ///
    /// # Errors
    ///
    /// Returns the reason the event could not be recorded.
    fn record(&self, event: &VaultRead<'_>) -> std::result::Result<(), String>;
}

/// The devices a store records reads to.
///
/// The built-in one is not in this list and is not removable: a trail that can
/// be emptied by configuration is a trail an operator can switch off by
/// accident, and the store would then serve secrets recording nothing while
/// every dashboard reported health.
#[derive(Debug, Default)]
pub struct AuditTrail {
    installed: RwLock<Vec<Arc<dyn AuditDevice>>>,
}

impl AuditTrail {
    /// Install a device. Every installed device must succeed for a read to be
    /// served.
    pub fn install(&self, device: Arc<dyn AuditDevice>) {
        if let Ok(mut installed) = self.installed.write() {
            installed.push(device);
        }
    }

    /// Record a read, everywhere, before its answer leaves.
    ///
    /// # Errors
    ///
    /// Returns [`Error::AuditUnavailable`] when the built-in trail cannot be
    /// written or any installed device refuses. The caller must treat that as a
    /// refusal of the read it was about to serve.
    pub fn record(&self, store: &Store, event: &VaultRead<'_>) -> Result<()> {
        write_entry(store, event)?;
        let installed = self
            .installed
            .read()
            .map_err(|_| Error::AuditUnavailable {
                reason: "the device list is poisoned".to_owned(),
            })?
            .clone();
        for device in installed {
            device
                .record(event)
                .map_err(|reason| Error::AuditUnavailable { reason })?;
        }
        Ok(())
    }
}

/// Every recorded read, oldest first.
///
/// # The forensic question
///
/// This exists to answer one question — *this credential was compromised at
/// time T; what did it read, and what must now be rotated?* — and it answers it
/// by scanning, which is the honest shape at this size and stops being one long
/// before a busy store's trail does. That limit is named rather than designed
/// around now: an index over the trail is a later addition, and pretending the
/// scan is a plan would be worse than saying it is not.
///
/// There is no statement for this yet, so an operator answers the question
/// through a program rather than through the language. That is a real gap and it
/// is recorded as one.
///
/// # Errors
///
/// Returns an error when the trail cannot be read.
pub fn entries(store: &Store) -> Result<Vec<Value>> {
    let transaction = store.begin()?;
    let mut found = Vec::new();
    for (_, payload) in transaction.scan_table(
        system::SYSTEM_NAMESPACE,
        system::SYSTEM_DATABASE,
        system::VAULT_AUDIT,
    )? {
        found.push(tessari_encoding::decode_payload(&payload)?);
    }
    Ok(found)
}

/// What one actor read, oldest first.
///
/// # Errors
///
/// Returns an error when the trail cannot be read.
pub fn reads_by(store: &Store, actor: &str) -> Result<Vec<Value>> {
    Ok(entries(store)?
        .into_iter()
        .filter(|entry| match entry {
            Value::Object(fields) => fields.get("actor") == Some(&Value::String(actor.to_owned())),
            _ => false,
        })
        .collect())
}

/// Write one entry into the store's own trail, in its own transaction.
///
/// **Its own** and not the reader's, which is the point rather than a detail: a
/// `REVEAL` inside a transaction the caller then cancels would otherwise hand
/// back plaintext and roll the record of it away, leaving exactly the gap the
/// ordering rule exists to close.
fn write_entry(store: &Store, event: &VaultRead<'_>) -> Result<()> {
    let mut transaction = store.begin()?;
    let address = system::address(system::VAULT_AUDIT, RecordId::Uuid(entry_id()?));
    transaction.put(
        address,
        tessari_encoding::encode_payload(&entry(event)).into_bytes(),
    );
    transaction
        .commit()
        .map(|_| ())
        .map_err(|error| Error::AuditUnavailable {
            reason: error.to_string(),
        })
}

/// The entry as the catalog holds it.
fn entry(event: &VaultRead<'_>) -> Value {
    Value::Object(
        [
            ("at".to_owned(), Value::Datetime(now())),
            ("actor".to_owned(), Value::String(event.actor.to_owned())),
            (
                "namespace".to_owned(),
                Value::Number(i64::from(event.namespace.get()).into()),
            ),
            (
                "database".to_owned(),
                Value::Number(i64::from(event.database.get()).into()),
            ),
            ("vault".to_owned(), Value::String(event.vault.to_owned())),
            ("record".to_owned(), Value::String(event.record.to_owned())),
            (
                "fields".to_owned(),
                Value::Array(
                    event
                        .fields
                        .iter()
                        .map(|field| Value::String(field.clone()))
                        .collect(),
                ),
            ),
            ("served".to_owned(), Value::Bool(event.served)),
        ]
        .into_iter()
        .collect(),
    )
}

/// A time-ordered identity for an entry.
///
/// Version 7 so the trail reads in the order it was written without a counter —
/// and a counter is what this deliberately avoids, because one shared key per
/// entry would serialise every concurrent read through a write conflict.
fn entry_id() -> Result<[u8; 16]> {
    // Random first, then the timestamp over the leading six bytes: the ten that
    // survive are what distinguish entries written in the same millisecond,
    // which under load is most of them.
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(|_| Error::AuditUnavailable {
        reason: "the operating system's randomness source could not be read".to_owned(),
    })?;
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| Error::AuditUnavailable {
            reason: "the system clock reads a time before 1970".to_owned(),
        })?
        .as_millis();
    let [_, _, t0, t1, t2, t3, t4, t5] = u64::try_from(millis).unwrap_or(u64::MAX).to_be_bytes();
    bytes[0] = t0;
    bytes[1] = t1;
    bytes[2] = t2;
    bytes[3] = t3;
    bytes[4] = t4;
    bytes[5] = t5;
    bytes[6] = (bytes[6] & 0x0f) | 0x70;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Ok(bytes)
}

/// The wall clock, as a value.
fn now() -> Datetime {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|since| Datetime::new(i64::try_from(since.as_secs()).ok()?, since.subsec_nanos()))
        .unwrap_or_else(|| Datetime::from_seconds(0))
}
