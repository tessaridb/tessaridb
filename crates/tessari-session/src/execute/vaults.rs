//! Vaults: declaring, unsealing, recipients and reading secrets.

use std::collections::BTreeMap;
use tessari_encoding::{decode_payload, encode_payload};
use tessari_ql::{Name, RecordTarget, Span, TableRef};
use tessari_storage::{
    Catalog, RecordAddress, TableDefinition, TableKind, TableShape, Transaction, VaultCustody,
    VaultDeclaration,
};

use tessari_types::{IdentityKind, TableId, Value};

use crate::error::{Error, Result};
use crate::outcome::Outcome;
use crate::session::Session;

impl Session<'_> {
    /// `DROP GEO places` — the store's definition, its geometry field and its
    /// spatial index declaration, and not its records; see
    /// [`Session::drop_vector`] for why the distinction is written down.
    ///
    /// Refuses a table that is not one, for the reason [`Session::drop_vector`]
    /// does: the words name different things even where they would remove the
    /// same rows, and a `DROP GEO` that quietly removed an ordinary table would
    /// be a typo with the blast radius of a table.
    /// `DEFINE VAULT team` · `DEFINE VAULT team PASSPHRASE '…'`
    ///
    /// A table of the vault kind, carrying a key minted here and wrapped under
    /// the store's master key — or, given a passphrase, under a key derived
    /// from it (ADR-0093), which needs no unsealed store at all. That is why this is the **one** declaration that
    /// needs an unsealed store: there is no way to defer the key without
    /// creating a vault nothing can ever write to, and a declaration that
    /// succeeded and left the key for later would be a vault that refuses every
    /// write while `INFO` reports it as ready.
    pub(super) fn define_vault(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        if_not_exists: bool,
        passphrase: Option<&str>,
        span: Span,
    ) -> Result<Outcome> {
        // The scope is computed by the storage layer's own function, not
        // rebuilt here. The write path recomputes the same binding from the
        // stored definition, and two implementations of one binding produce a
        // vault that accepts every write and opens nothing, with both halves
        // looking correct in isolation.
        let custody = match passphrase {
            Some(passphrase) => self.own_vault_custody(transaction, name, passphrase, span)?,
            None => {
                let context = self.context(transaction, None, span)?;
                VaultCustody::Store(tessari_storage::mint_vault_key(
                    self.store,
                    context.namespace,
                    context.database,
                    &name.text,
                )?)
            }
        };
        self.define_table(
            transaction,
            name,
            TableShape {
                // **Strict**, unlike every other declared store, and this is the
                // one place the default is wrong rather than merely different.
                //
                // What seals a field is the `SECRET` marker on its declaration.
                // A field nobody declared carries no marker, so in a schemaless
                // vault it is accepted and written in the clear — beside the
                // sealed fields, inside the store whose whole promise is that it
                // holds nothing readable. The caller doing it is doing the most
                // ordinary thing a schemaless store allows, and believes the
                // record is protected because the record is in a vault.
                //
                // An earlier comment here argued that `SCHEMAFULL` would be a
                // second thing to remember for a property it does not provide.
                // It provides exactly one property and this is it: strictness is
                // what makes *declared* and *sealed* the same set.
                schemafull: true,
                kind: TableKind::Vault(VaultDeclaration { custody }),
                identity: IdentityKind::default(),
                graph: None,
                conflict: None,
                split: Vec::new(),
                partition: None,
                spread: false,
            },
            if_not_exists,
            span,
        )
    }

    /// `DROP VAULT team` — the crypto-shred.
    ///
    /// Dropping the definition destroys the wrapped key with it, and the key is
    /// the only copy: every record of this vault in every backup, snapshot and
    /// replica that will ever be restored becomes ciphertext under a key that
    /// exists nowhere. That is the deletion claim, and it is the only one a
    /// store like this can honestly make — a row delete says something about the
    /// live table and nothing about the data.
    ///
    /// It does **not** require an unsealed store. Destroying a key needs no key,
    /// and demanding one would mean a store that cannot be unsealed can never be
    /// cleaned up — which is exactly the store an operator most wants to shred.
    pub(super) fn drop_vault(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        span: Span,
    ) -> Result<Outcome> {
        let context = self.context(transaction, None, span)?;
        let missing = || Error::Unknown {
            entity: "vault",
            name: name.text.clone(),
            span,
        };
        let id = Catalog::new(transaction)
            .table_id(context.namespace, context.database, &name.text)?
            .ok_or_else(missing)?;
        let is_vault = Catalog::new(transaction)
            .table(id)?
            .is_some_and(|definition| definition.is_vault());
        if !is_vault {
            return Err(missing());
        }
        Catalog::new(transaction).drop_table(id)?;
        Ok(Outcome::Done)
    }

    /// `UNSEAL VAULT WITH '…'` — the master key enters this process.
    ///
    /// # The first unseal creates the store's root, and says so
    ///
    /// A store that has never held a secret has no root record, and something
    /// has to make one. Rather than add a second statement for a once-in-a-store
    /// act, this one initialises when there is nothing to unlock — and the
    /// outcome says **which** of the two happened, because the hazard here is
    /// that a mistyped passphrase on an empty store becomes the passphrase, and
    /// there is deliberately no path that replaces a root once written.
    ///
    /// Saying which happened is what makes that hazard survivable: an operator
    /// who expected *unsealed* and reads *initialised* knows immediately, while
    /// the store still holds nothing. Q-415 carries the open question of whether
    /// initialisation should be its own statement anyway.
    pub(super) fn unseal_vault(
        &self,
        transaction: &mut Transaction<'_>,
        passphrase: &str,
        span: Span,
    ) -> Result<Outcome> {
        let _ = span;
        if let Some(root) = Catalog::new(transaction).vault_root()? {
            unseal_throttled(self.store, &root, passphrase)?;
            return Ok(Outcome::Value(Value::from("unsealed")));
        }
        let root = tessari_storage::initialise_root(self.store, passphrase)?;
        Catalog::new(transaction).set_vault_root(&root);
        Ok(Outcome::Value(Value::from("initialised")))
    }

    /// `CHANGE VAULT PASSPHRASE FROM '…' TO '…'` — a rekey (ADR-0092 D3).
    ///
    /// The current passphrase is checked under the unseal's own throttle, then
    /// the same master key is wrapped under the new one and the root record
    /// replaced in this transaction. Nothing else moves: the process stays
    /// sealed or unsealed as it was, and every secret keeps its bytes.
    pub(super) fn change_passphrase(
        &self,
        transaction: &mut Transaction<'_>,
        current: &str,
        new: &str,
        span: Span,
    ) -> Result<Outcome> {
        let _ = span;
        let Some(root) = Catalog::new(transaction).vault_root()? else {
            return Err(Error::NoVaultRoot);
        };
        let moved = guessed(self.store, &root, || {
            root.0
                .rewrap(current, new)
                .map_err(tessari_storage::Error::Vault)
        })?;
        Catalog::new(transaction).set_vault_root(&tessari_storage::VaultRoot(moved));
        tracing::info!("the vault passphrase was changed");
        Ok(Outcome::Done)
    }

    /// `REVEAL password FROM team:github` — the only path to a plaintext.
    ///
    /// Reads the stored record, opens the record's data key, and opens each
    /// named secret field under it. Every other read path in this store sees
    /// what is on disk, which is ciphertext.
    ///
    /// # What it refuses, and why each refusal is here rather than in the parser
    ///
    /// A field that is not declared `SECRET` is refused rather than returned in
    /// the clear. `REVEAL` answers with plaintext, so a caller reading its answer
    /// has no way to tell which entries were ever sealed — and a verb that
    /// sometimes returns a secret and sometimes returns whatever was lying about
    /// is one whose output nobody can reason about. The parser cannot make this
    /// refusal because it does not know what any name refers to.
    ///
    /// A table that is not a vault is refused for the same reason: `REVEAL` over
    /// an ordinary table would be a `SELECT` wearing a word that promises more.
    /// Resolve a record in a vault, for the three statements that name one.
    ///
    /// A table that is not a vault is reported as **no such vault** rather than
    /// as a wrong kind, which is the same answer `REVEAL` gives: a caller who
    /// may not reach a table learns nothing from these verbs that `SELECT`
    /// would not have told them, and one who may reach it gets a message naming
    /// the word they should have used.
    pub(crate) fn vault_record(
        &self,
        transaction: &mut Transaction<'_>,
        target: &RecordTarget,
        span: Span,
    ) -> Result<(RecordAddress, TableDefinition, BTreeMap<String, Value>)> {
        let missing = || Error::Unknown {
            entity: "vault",
            name: target.table.name.text.clone(),
            span,
        };
        let (_context, address) = self.address(transaction, target)?;
        let definition = Catalog::new(transaction)
            .table(address.table)?
            .ok_or_else(missing)?;
        if !definition.is_vault() {
            return Err(missing());
        }
        let Some(stored) = transaction.get(&address)? else {
            return Err(Error::NoSuchRecord {
                id: address.id.to_string(),
                span,
            });
        };
        let Value::Object(held) = decode_payload(&stored)? else {
            return Err(missing());
        };
        Ok((address, definition, held))
    }

    /// A recipient's name, which must be text.
    ///
    /// Anything else is refused by **type** — never by value. A caller who wrote
    /// a field reference here would otherwise have the store quote whatever that
    /// field holds back at them, and on a vault's record that is the one thing
    /// this feature exists to keep unquoted.
    pub(super) fn recipient_name(
        &self,
        transaction: &mut Transaction<'_>,
        expression: &tessari_ql::Expr,
        span: Span,
    ) -> Result<String> {
        match self.evaluate(transaction, expression)? {
            Value::String(name) => Ok(name),
            other => Err(Error::RecipientIsNotAName {
                found: other.type_name(),
                span,
            }),
        }
    }

    /// Add or remove one entry in a record's recipient set.
    ///
    /// # Why this does not go through `put_record`
    ///
    /// Two reasons, and both are structural rather than stylistic. The write
    /// path **refuses** a payload carrying the reserved key set at all, so this
    /// change cannot be expressed as an ordinary write. And a write through it
    /// re-seals: a fresh data key and fresh nonces for every secret field, so
    /// every ciphertext on the record would change — which is precisely what
    /// criterion F2 forbids, and what would make an added recipient
    /// indistinguishable from a rewritten secret in a backup diff.
    ///
    /// Nothing is re-indexed, and that is correct rather than an omission: the
    /// only field this touches is the key set, an index over a secret field is
    /// refused at declaration, and no indexed value changes. The test that
    /// holds this true reads through an index on a plain field after a
    /// recipient is added.
    pub(super) fn change_recipients(
        &self,
        transaction: &mut Transaction<'_>,
        target: &RecordTarget,
        span: Span,
        change: impl FnOnce(
            &mut BTreeMap<String, Value>,
            &str,
        ) -> std::result::Result<(), tessari_storage::Error>,
    ) -> Result<Outcome> {
        let (address, definition, mut held) = self.vault_record(transaction, target, span)?;
        change(&mut held, &definition.name)?;
        transaction.put(address, encode_payload(&Value::Object(held)).into_bytes());
        Ok(Outcome::Done)
    }

    /// `REVEAL` — and the record of it, written before the answer leaves.
    ///
    /// # Why the audit is here and not inside the opening
    ///
    /// Because the property is about ORDER, and order is only visible from the
    /// place that owns both events. Written afterwards, every crash, kill,
    /// timeout and partial write between the decryption and the log produces a
    /// secret release with no record — and the two orderings are
    /// indistinguishable whenever nothing fails, which is why the defect
    /// survives review.
    ///
    /// The refusal is recorded too. A denial is the reconnaissance signal: the
    /// first evidence of somebody probing what exists and what they can reach,
    /// and without it the earliest thing the trail shows is a successful read,
    /// which is the point at which the damage is already done.
    ///
    /// A trail that cannot be written **refuses**, including refusing to report
    /// the refusal it was trying to record. That is deliberate: the alternative
    /// leaks whether a record exists to a caller who has disabled the trail.
    pub(super) fn reveal(
        &self,
        transaction: &mut Transaction<'_>,
        target: &RecordTarget,
        fields: &[Name],
        span: Span,
    ) -> Result<Outcome> {
        let (_context, address) = self.address(transaction, target)?;
        let opened = self.open_secrets(transaction, target, fields, span);
        let asked: Vec<String> = fields.iter().map(|field| field.text.clone()).collect();
        let record = address.id.to_literal();
        self.store.audit().record(
            self.store,
            &tessari_storage::VaultRead {
                actor: self
                    .identity
                    .user()
                    .map_or("anonymous", |user| user.name.as_str()),
                namespace: address.namespace,
                database: address.database,
                vault: &target.table.name.text,
                record: &record,
                fields: &asked,
                served: opened.is_ok(),
            },
        )?;
        opened
    }

    /// The opening itself, with no knowledge that it is being recorded.
    pub(super) fn open_secrets(
        &self,
        transaction: &mut Transaction<'_>,
        target: &RecordTarget,
        fields: &[Name],
        span: Span,
    ) -> Result<Outcome> {
        let missing = || Error::Unknown {
            entity: "vault",
            name: target.table.name.text.clone(),
            span,
        };
        let (_context, address) = self.address(transaction, target)?;
        let id = address.table;
        let definition = Catalog::new(transaction).table(id)?.ok_or_else(missing)?;
        if !definition.is_vault() {
            return Err(missing());
        }

        let secrets: BTreeMap<String, ()> = Catalog::new(transaction)
            .fields_on(id)?
            .into_iter()
            .filter(|field| field.secret)
            .map(|field| (field.name, ()))
            .collect();

        // Named fields are checked against the declaration BEFORE the record is
        // read, so a caller cannot use the difference between "no such field"
        // and "no such record" to learn which records exist.
        let wanted: Vec<String> = if fields.is_empty() {
            secrets.keys().cloned().collect()
        } else {
            for field in fields {
                if !secrets.contains_key(&field.text) {
                    return Err(Error::NotASecret {
                        field: field.text.clone(),
                        vault: target.table.name.text.clone(),
                        span,
                    });
                }
            }
            fields.iter().map(|field| field.text.clone()).collect()
        };

        let Some(stored) = transaction.get(&address)? else {
            return Ok(Outcome::Value(Value::None));
        };
        let Value::Object(held) = decode_payload(&stored)? else {
            return Err(missing());
        };

        let data_key = tessari_storage::open_data_key(transaction, &address, &definition, &held)?;
        let mut opened = BTreeMap::new();
        for name in wanted {
            let Some(Value::Bytes(envelope)) = held.get(&name) else {
                // Declared secret, absent from this record. Reported as absent
                // rather than skipped: a caller who asked for three fields and
                // got two has no way to tell which one was missing.
                opened.insert(name, Value::None);
                continue;
            };
            let value =
                tessari_storage::open_field(&data_key, &address, &definition, &name, envelope)?;
            opened.insert(name, value);
        }
        Ok(Outcome::Value(Value::Object(opened)))
    }

    /// Refuse an index whose fields include one the vault seals.
    ///
    /// Checked against the **declaration** rather than against any record, so a
    /// vault with no rows yet refuses exactly as one with a million does. The
    /// alternative — noticing at index-build time — would accept the statement
    /// and fail later, by which point the declaration is in the catalog and the
    /// failure looks like the data's fault.
    pub(super) fn refuse_indexing_a_secret(
        &self,
        transaction: &mut Transaction<'_>,
        id: TableId,
        table: &TableRef,
        fields: &[tessari_ql::FieldPath],
    ) -> Result<()> {
        if !Catalog::new(transaction)
            .table(id)?
            .is_some_and(|definition| definition.is_vault())
        {
            return Ok(());
        }
        let secrets: Vec<String> = Catalog::new(transaction)
            .fields_on(id)?
            .into_iter()
            .filter(|field| field.secret)
            .map(|field| field.name)
            .collect();
        for field in fields {
            // The **root** of the path, because indexing `password.length` is
            // indexing the secret just as surely as indexing `password` is — it
            // is a projection of the plaintext, and a projection of a plaintext
            // is a plaintext somebody derived.
            let root = field.path.root();
            if secrets.iter().any(|secret| secret == root) {
                return Err(Error::NotIndexable {
                    field: root.to_owned(),
                    table: table.name.text.clone(),
                    span: table.span,
                });
            }
        }
        Ok(())
    }
}

/// Unseal with `passphrase`, under the bounds sign-in has (ADR-0092 D2).
pub(crate) fn unseal_throttled(
    store: &tessari_storage::Store,
    root: &tessari_storage::VaultRoot,
    passphrase: &str,
) -> Result<()> {
    guessed(store, root, || store.vault().unseal(&root.0, passphrase))
}

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
pub(super) fn guessed<T>(
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

/// The throttle's key for guesses at one store's passphrase.
///
/// Starts with a character no user name can hold, so it never shares a count
/// with somebody's sign-in by construction rather than by luck of the hash.
fn passphrase_key(root: &tessari_storage::VaultRoot) -> String {
    let mut key = String::from("\u{0}passphrase:");
    for byte in root.0.salt {
        key.push_str(&format!("{byte:02x}"));
    }
    key
}
