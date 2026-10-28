//! Vaults carrying their own passphrase (ADR-0093): declaring one, and the
//! named `UNSEAL`, `SEAL` and `CHANGE` that act on it alone.

use tessari_ql::{Name, Span};
use tessari_storage::{
    Catalog, TableDefinition, Transaction, VaultCustody, VaultDeclaration, VaultRoot,
};
use tessari_types::{TableId, Value};

use super::vaults::guessed;
use crate::error::{Error, Result};
use crate::outcome::Outcome;
use crate::session::Session;

impl Session<'_> {
    /// The key of `DEFINE VAULT team PASSPHRASE '…'`: a vault key under the
    /// vault's own passphrase, held in this process for one period.
    pub(super) async fn own_vault_custody(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        passphrase: &str,
        span: Span,
    ) -> Result<VaultCustody> {
        let context = self.context(transaction, None, span).await?;
        let root = tessari_storage::mint_own_vault_key(
            self.store,
            context.namespace,
            context.database,
            &name.text,
            passphrase,
        )?;
        Ok(VaultCustody::Own(root))
    }

    /// The key of a vault declared over the vault surface with a presented key (ADR-0136): a vault key wrapped under
    /// that key, held nowhere afterwards.
    pub(super) async fn keyed_vault_custody(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        key: &tessari_ql::VaultKey,
        span: Span,
    ) -> Result<VaultCustody> {
        let context = self.context(transaction, None, span).await?;
        let presented = tessari_vault::SecretBytes::adopt(*key.expose());
        let root = tessari_storage::mint_keyed_vault_key(
            context.namespace,
            context.database,
            &name.text,
            &presented,
        )?;
        Ok(VaultCustody::Keyed(root))
    }

    /// Re-wrap a keyed vault's key under a new presented key, under that vault's throttle, rewriting its declaration
    /// in this transaction (ADR-0136).
    pub(super) async fn rewrap_vault_key(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        current: &tessari_ql::VaultKey,
        new: &tessari_ql::VaultKey,
        span: Span,
    ) -> Result<Outcome> {
        let (id, definition, root) = self.keyed_vault(transaction, name, span).await?;
        let current = tessari_vault::SecretBytes::adopt(*current.expose());
        let new = tessari_vault::SecretBytes::adopt(*new.expose());
        let moved = guessed(self.store, &root, || {
            tessari_storage::rewrap_keyed_vault(&definition, &root.0, &current, &new)
        })?;
        Catalog::new(transaction)
            .set_vault(
                id,
                VaultDeclaration {
                    custody: VaultCustody::Keyed(moved),
                },
            )
            .await?;
        tracing::info!("a vault key was changed");
        Ok(Outcome::Done)
    }

    /// Check a key presented for `name` against that vault's root, under its throttle, and answer the scope it opens
    /// (ADR-0136). Run before any statement of the request, so a wrong key is refused before anything is read.
    pub(crate) async fn check_presented(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        key: &tessari_vault::SecretBytes,
        span: Span,
    ) -> Result<Vec<u8>> {
        let (_, definition, root) = self.keyed_vault(transaction, name, span).await?;
        let scope = tessari_storage::vault_key_scope(
            definition.namespace,
            definition.database,
            &definition.name,
        );
        guessed(self.store, &root, || {
            root.0
                .unlock_keyed_vault(key, &scope)
                .map(drop)
                .map_err(tessari_storage::Error::Vault)
        })?;
        Ok(scope)
    }

    /// A vault by name in keyed custody, or the refusal saying it is not one.
    async fn keyed_vault(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        span: Span,
    ) -> Result<(TableId, TableDefinition, VaultRoot)> {
        let (id, definition) = self.named_vault(transaction, name, span).await?;
        match definition.vault_custody() {
            Some(VaultCustody::Keyed(root)) => {
                let root = VaultRoot(root.clone());
                Ok((id, definition, root))
            }
            _ => Err(Error::VaultNotKeyed {
                vault: name.text.clone(),
                span,
            }),
        }
    }

    /// `UNSEAL VAULT team WITH '…'`
    pub(super) async fn unseal_named_vault(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        passphrase: &str,
        span: Span,
    ) -> Result<Outcome> {
        let (_, definition, root) = self.own_vault(transaction, name, span).await?;
        guessed(self.store, &root, || {
            tessari_storage::unseal_own_vault(self.store, &definition, &root.0, passphrase)
        })?;
        Ok(Outcome::Value(Value::from("unsealed")))
    }

    /// `SEAL VAULT team`
    pub(super) async fn seal_named_vault(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        span: Span,
    ) -> Result<Outcome> {
        let (_, _, root) = self.own_vault(transaction, name, span).await?;
        self.store.vault().seal_own(root.0.key_id)?;
        Ok(Outcome::Done)
    }

    /// `CHANGE VAULT team PASSPHRASE FROM '…' TO '…'` — a rekey of one vault,
    /// under that vault's throttle, rewriting its declaration in this
    /// transaction. The key it holds in memory, if any, is untouched.
    pub(super) async fn change_named_vault_passphrase(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        current: &str,
        new: &str,
        span: Span,
    ) -> Result<Outcome> {
        let (id, definition, root) = self.own_vault(transaction, name, span).await?;
        let moved = guessed(self.store, &root, || {
            tessari_storage::rewrap_own_vault(&definition, &root.0, current, new)
        })?;
        Catalog::new(transaction)
            .set_vault(
                id,
                VaultDeclaration {
                    custody: VaultCustody::Own(moved),
                },
            )
            .await?;
        tracing::info!("a vault passphrase was changed");
        Ok(Outcome::Done)
    }

    /// A vault by name, in the session's tenancy, with its custody.
    pub(crate) async fn named_vault(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        span: Span,
    ) -> Result<(TableId, TableDefinition)> {
        let context = self.context(transaction, None, span).await?;
        let missing = || Error::Unknown {
            entity: "vault",
            name: name.text.clone(),
            span,
        };
        let id = Catalog::new(transaction)
            .table_id(context.namespace, context.database, &name.text)
            .await?
            .ok_or_else(missing)?;
        let definition = Catalog::new(transaction)
            .table(id)
            .await?
            .filter(TableDefinition::is_vault)
            .ok_or_else(missing)?;
        Ok((id, definition))
    }

    /// A vault by name that carries its own passphrase, or the refusal saying it
    /// opens with the store's.
    ///
    /// The root comes back in the catalog's wrapper because that is what the
    /// throttle keys its count by, as it does the store's.
    async fn own_vault(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        span: Span,
    ) -> Result<(TableId, TableDefinition, VaultRoot)> {
        let (id, definition) = self.named_vault(transaction, name, span).await?;
        match definition.vault_custody() {
            Some(VaultCustody::Own(root)) => {
                let root = VaultRoot(root.clone());
                Ok((id, definition, root))
            }
            Some(VaultCustody::Keyed(_)) => Err(Error::VaultOpensWithAKey {
                vault: name.text.clone(),
                span,
            }),
            _ => Err(Error::VaultUsesStorePassphrase {
                vault: name.text.clone(),
                span,
            }),
        }
    }
}
