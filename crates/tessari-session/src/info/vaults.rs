//! INFO for a vault and its recipients.

use std::collections::BTreeMap;

use tessari_ql::{Name, RecordTarget, Span};
use tessari_storage::{Catalog, Transaction};
use tessari_types::Value;

use crate::error::{Error, Result};
use crate::session::Session;

impl Session<'_> {
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
