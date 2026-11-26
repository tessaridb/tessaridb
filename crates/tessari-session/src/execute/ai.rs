//! `DEFINE AI PROVIDER` and its drop (G102 C8): the provider as a catalog
//! object, its key named as a vault secret and never written.
//!
//! The declare-time checks are the design's sealing story made mechanical:
//! the endpoint is `https` (the parser already refused plaintext, and the
//! catalog re-checks on read), the vault named must exist — a store with no
//! vault cannot declare a provider, because a key with nowhere sealed to
//! live would have to live somewhere else — and the format gate holds the
//! statement off a store an older build could still open, whose key grammar
//! cannot place the row.

use tessari_ql::{Name, Span};
use tessari_storage::{AiProviderDefinition, Catalog, Transaction};

use crate::error::{Error, Result};
use crate::outcome::Outcome;
use crate::session::Session;

/// What `DEFINE AI PROVIDER` stated, borrowed from the statement.
pub(super) struct StatedProvider<'a> {
    /// The endpoint.
    pub(super) url: &'a str,
    /// The model.
    pub(super) model: &'a str,
    /// The vault the key is sealed in.
    pub(super) vault: &'a Name,
    /// The secret's name in that vault.
    pub(super) secret: &'a Name,
    /// Calls per span.
    pub(super) budget: u64,
    /// The span, in seconds.
    pub(super) budget_span_seconds: u64,
    /// One call's ceiling, in seconds.
    pub(super) timeout_seconds: u64,
}

impl Session<'_> {
    /// `DEFINE AI PROVIDER anthropic URL '…' MODEL '…' KEY VAULT v FIELD f
    /// BUDGET n PER 1h TIMEOUT 10s`.
    pub(super) async fn define_ai_provider(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        stated: &StatedProvider<'_>,
        if_not_exists: bool,
        span: Span,
    ) -> Result<Outcome> {
        self.refuse_a_format_the_store_does_not_hold(
            "an AI provider",
            tessari_encoding::FormatVersion::AI_PROVIDERS,
        )
        .await?;
        let context = self.context(transaction, None, span).await?;
        // The vault must exist and be one: the key has nowhere sealed to
        // live otherwise, and a provider whose key lives nowhere is a
        // plaintext key waiting to be written somewhere else.
        let missing = || Error::AiKeyNotSealed {
            provider: name.text.clone(),
            reason: "names a vault this database does not hold",
            span,
        };
        let id = Catalog::new(transaction)
            .table_id(context.namespace, context.database, &stated.vault.text)
            .await?
            .ok_or_else(missing)?;
        let is_vault = Catalog::new(transaction)
            .table(id)
            .await?
            .is_some_and(|definition| definition.is_vault());
        if !is_vault {
            return Err(missing());
        }
        let mut catalog = Catalog::new(transaction);
        if if_not_exists
            && catalog
                .ai_provider(context.namespace, &name.text)
                .await?
                .is_some()
        {
            return Ok(Outcome::Done);
        }
        catalog
            .create_ai_provider(&AiProviderDefinition {
                namespace: context.namespace,
                name: name.text.clone(),
                url: stated.url.to_owned(),
                model: stated.model.to_owned(),
                vault: stated.vault.text.clone(),
                secret: stated.secret.text.clone(),
                budget: stated.budget,
                budget_span_seconds: stated.budget_span_seconds,
                timeout_seconds: stated.timeout_seconds,
            })
            .await?;
        Ok(Outcome::Done)
    }

    /// `DROP AI PROVIDER anthropic` — the provider whole; its calls refuse
    /// `AiNoProvider` from then on.
    pub(super) async fn drop_ai_provider(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        span: Span,
    ) -> Result<Outcome> {
        let namespace = self.namespace_id(transaction, span).await?;
        if !Catalog::new(transaction)
            .drop_ai_provider(namespace, &name.text)
            .await?
        {
            return Err(Error::Unknown {
                entity: "AI provider",
                name: name.text.clone(),
                span,
            });
        }
        Ok(Outcome::Done)
    }
}
