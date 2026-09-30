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
    pub(super) fn own_vault_custody(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        passphrase: &str,
        span: Span,
    ) -> Result<VaultCustody> {
        let context = self.context(transaction, None, span)?;
        let root = tessari_storage::mint_own_vault_key(
            self.store,
            context.namespace,
            context.database,
            &name.text,
            passphrase,
        )?;
        Ok(VaultCustody::Own(root))
    }

    /// `UNSEAL VAULT team WITH '…'`
    pub(super) fn unseal_named_vault(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        passphrase: &str,
        span: Span,
    ) -> Result<Outcome> {
        let (_, definition, root) = self.own_vault(transaction, name, span)?;
        guessed(&root, || {
            tessari_storage::unseal_own_vault(self.store, &definition, &root.0, passphrase)
        })?;
        Ok(Outcome::Value(Value::from("unsealed")))
    }

    /// `SEAL VAULT team`
    pub(super) fn seal_named_vault(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        span: Span,
    ) -> Result<Outcome> {
        let (_, _, root) = self.own_vault(transaction, name, span)?;
        self.store.vault().seal_own(root.0.key_id)?;
        Ok(Outcome::Done)
    }

    /// `CHANGE VAULT team PASSPHRASE FROM '…' TO '…'` — a rekey of one vault,
    /// under that vault's throttle, rewriting its declaration in this
    /// transaction. The key it holds in memory, if any, is untouched.
    pub(super) fn change_named_vault_passphrase(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        current: &str,
        new: &str,
        span: Span,
    ) -> Result<Outcome> {
        let (id, definition, root) = self.own_vault(transaction, name, span)?;
        let moved = guessed(&root, || {
            tessari_storage::rewrap_own_vault(&definition, &root.0, current, new)
        })?;
        Catalog::new(transaction).set_vault(
            id,
            VaultDeclaration {
                custody: VaultCustody::Own(moved),
            },
        )?;
        log::info!("a vault passphrase was changed");
        Ok(Outcome::Done)
    }

    /// A vault by name, in the session's tenancy, with its custody.
    pub(crate) fn named_vault(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        span: Span,
    ) -> Result<(TableId, TableDefinition)> {
        let context = self.context(transaction, None, span)?;
        let missing = || Error::Unknown {
            entity: "vault",
            name: name.text.clone(),
            span,
        };
        let id = Catalog::new(transaction)
            .table_id(context.namespace, context.database, &name.text)?
            .ok_or_else(missing)?;
        let definition = Catalog::new(transaction)
            .table(id)?
            .filter(TableDefinition::is_vault)
            .ok_or_else(missing)?;
        Ok((id, definition))
    }

    /// A vault by name that carries its own passphrase, or the refusal saying it
    /// opens with the store's.
    ///
    /// The root comes back in the catalog's wrapper because that is what the
    /// throttle keys its count by, as it does the store's.
    fn own_vault(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        span: Span,
    ) -> Result<(TableId, TableDefinition, VaultRoot)> {
        let (id, definition) = self.named_vault(transaction, name, span)?;
        match definition.vault_custody() {
            Some(VaultCustody::Own(root)) => {
                let root = VaultRoot(root.clone());
                Ok((id, definition, root))
            }
            _ => Err(Error::VaultUsesStorePassphrase {
                vault: name.text.clone(),
                span,
            }),
        }
    }
}
