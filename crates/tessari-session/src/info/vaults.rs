//! INFO for a vault and its recipients.

use std::collections::BTreeMap;

use tessari_ql::{Name, RecordTarget, Span, TableRef};
use tessari_storage::{Catalog, SealState, Transaction};
use tessari_types::Value;

use crate::error::{Error, Result};
use crate::session::Session;

impl Session<'_> {
    /// `INFO FOR SEAL` — `uninitialised`, `sealed` or `unsealed`, when an
    /// unsealed store seals itself, and how long an unseal lasts here.
    ///
    /// `uninitialised` is read from the catalog and the other two from this
    /// process, which is the split the root record and the key already have:
    /// the root travels with the store, the key never leaves the process.
    pub(super) fn info_seal(
        &self,
        transaction: &mut Transaction<'_>,
    ) -> Result<BTreeMap<String, Value>> {
        let initialised = Catalog::new(transaction).vault_root()?.is_some();
        let (state, seals_at) = match self.store.vault().state() {
            _ if !initialised => ("uninitialised", Value::None),
            SealState::Sealed => ("sealed", Value::None),
            SealState::Unsealed { seals_at } => ("unsealed", instant(seals_at)),
        };
        let period = self.store.vault().period();
        let unseal_for = tessari_types::Duration::new(
            i64::try_from(period.as_secs()).unwrap_or(i64::MAX),
            period.subsec_nanos(),
        )
        .map_or(Value::None, Value::Duration);

        let mut report = BTreeMap::new();
        report.insert("state".to_owned(), Value::from(state));
        report.insert("seals_at".to_owned(), seals_at);
        report.insert("unseal_for".to_owned(), unseal_for);
        Ok(report)
    }

    pub(super) fn info_vault(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        span: Span,
    ) -> Result<BTreeMap<String, Value>> {
        let context = self.context(transaction, None, span)?;
        let missing = || Error::Unknown {
            entity: "vault",
            name: name.text.clone(),
            span,
        };
        let id = Catalog::new(transaction)
            .table_id(context.namespace, context.database, &name.text)?
            .ok_or_else(missing)?;
        let definition = Catalog::new(transaction).table(id)?.ok_or_else(missing)?;
        if !definition.is_vault() {
            return Err(missing());
        }
        let fields = Catalog::new(transaction)
            .fields_on(id)?
            .into_iter()
            .map(|field| {
                (
                    field.name,
                    Value::Object(BTreeMap::from([
                        ("type".to_owned(), Value::from(field.kind.name().as_ref())),
                        ("secret".to_owned(), Value::Bool(field.secret)),
                    ])),
                )
            })
            .collect();
        Ok(BTreeMap::from([
            ("name".to_owned(), Value::from(name.text.as_str())),
            ("fields".to_owned(), Value::Object(fields)),
        ]))
    }

    /// `INFO FOR VAULT team RECORDS [AFTER team:'x'] [LIMIT n]` — one page of
    /// the vault's record ids in key order, and `next` naming the last of them
    /// when the page was full (ADR-0092 D5).
    ///
    /// Read by the key walk that serves `SELECT … AFTER`, so a page begins at a
    /// position rather than at the table, and it reads the stored payloads only
    /// to step over them: no field of any record reaches the answer, sealed or
    /// not. `next` is absent when the page came back short, because a short page
    /// is the last one.
    pub(super) fn info_vault_records(
        &self,
        transaction: &mut Transaction<'_>,
        table: &TableRef,
        after: Option<&RecordTarget>,
        limit: Option<u64>,
        span: Span,
    ) -> Result<BTreeMap<String, Value>> {
        let missing = || Error::Unknown {
            entity: "vault",
            name: table.name.text.clone(),
            span,
        };
        let (context, id) = self.resolve_any_table(transaction, table)?;
        let is_vault = Catalog::new(transaction)
            .table(id)?
            .is_some_and(|definition| definition.is_vault());
        if !is_vault {
            return Err(missing());
        }
        let wanted = usize::try_from(limit.unwrap_or(VAULT_PAGE)).unwrap_or(usize::MAX);
        let page = match after {
            Some(anchor) => transaction.records_after(
                context.namespace,
                context.database,
                id,
                anchor.id.fixed(anchor.span)?,
                Some(wanted),
            )?,
            None => {
                transaction.first_records_of(context.namespace, context.database, id, wanted)?
            }
        };
        let ids: Vec<Value> = page.into_iter().map(|(held, _)| id_value(held)).collect();
        let next = if ids.len() >= wanted {
            ids.last().cloned().unwrap_or(Value::None)
        } else {
            Value::None
        };
        Ok(BTreeMap::from([
            ("records".to_owned(), Value::Array(ids)),
            ("next".to_owned(), next),
        ]))
    }

    /// `INFO FOR GEO places` — the store's field and its index.
    ///
    /// Shorter than [`Session::info_vector`] by exactly what the two engines
    /// differ by, and it stops being shorter than that. A vector store declares
    /// a width and a distance, so `INFO` reports both; a geo store declares
    /// nothing, so there is no parameter here to report.
    ///
    /// **But there is a measurement, and this once said there was not.** The
    /// earlier reasoning — a spatial index answers exactly, so nothing needs
    /// measuring — confused two different things. The *answer* is exact, because
    /// the predicate re-tests the real geometry above the index. The *filter* is
    /// not: it works on bounding boxes, and a box is not a geometry. How much it
    /// offers against how much survives is the health of the whole arrangement,
    /// and it is the one number that makes a structurally awkward row — a river,
    /// a road, a border, whose box is many times its own area — visible at all.
    /// `INFO FOR VAULT team` — which fields are sealed, and nothing more.
    ///
    /// The answer names each declared field and says whether it is `SECRET`. It
    /// does **not** carry a length, a fingerprint, a key identifier or a record
    /// count for the sealed ones, and that is a line rather than an omission: a
    /// length is an oracle that answers slowly, a key identifier tells an
    /// attacker which records share a key, and a reader would have no way to
    /// tell any of them was a disclosure.
    ///
    /// It does not report whether the store is sealed either. That is a property
    /// of this *process*, not of this vault, and answering it here would make a
    /// per-vault question out of a store-wide one.
    /// `INFO FOR RECIPIENTS OF team:github` — who may one day open this record.
    ///
    /// **Nothing here is filtered**, which is what keeps it safe to answer at
    /// all: the caller either holds the grant on the vault and sees the whole
    /// set, or is refused before this runs. A listing narrowed per caller would
    /// disclose by its size what it withheld by its contents, and this one has
    /// no size to read anything from.
    ///
    /// The material comes back with the names because it is the application's
    /// own ciphertext and the store never read it. What the store's own entry
    /// holds is not in the answer — see `tessari_storage::recipients`.
    pub(super) fn info_recipients(
        &self,
        transaction: &mut Transaction<'_>,
        target: &RecordTarget,
        span: Span,
    ) -> Result<BTreeMap<String, Value>> {
        let (_, definition, held) = self.vault_record(transaction, target, span)?;
        let entries = tessari_storage::recipients(&held, &definition.name)?;
        Ok(BTreeMap::from([(
            "recipients".to_owned(),
            Value::Object(entries),
        )]))
    }
}

/// A wall-clock instant as a datetime value, or nothing when it lies outside
/// what the clock can represent — never a made-up instant.
fn instant(at: std::time::SystemTime) -> Value {
    at.duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|since| {
            tessari_types::Datetime::new(i64::try_from(since.as_secs()).ok()?, since.subsec_nanos())
        })
        .map_or(Value::None, Value::Datetime)
}

/// How many ids a vault listing answers when it is not told (ADR-0092 D5).
const VAULT_PAGE: u64 = 1_000;

/// A record's identity as the value a caller writes after `team:` to name it
/// again — so the `next` of one page is the `AFTER team:$next` of the one after.
fn id_value(id: tessari_types::RecordId) -> Value {
    match id {
        tessari_types::RecordId::Int(number) => Value::from(number),
        tessari_types::RecordId::Text(text) => Value::String(text),
        tessari_types::RecordId::Uuid(bytes) => Value::Uuid(bytes),
        tessari_types::RecordId::Bytes(bytes) => Value::Bytes(bytes),
    }
}
