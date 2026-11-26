//! The AI providers as the log records them: one row per (namespace, name),
//! beside [`super::pool`]'s rows for the same reason — a provider is a
//! cluster-wide fact every node must agree about (G102 C8).
//!
//! # What a row holds, and the one thing it never does
//!
//! URL, model, the call budget and the timeout — and the NAME of the vault
//! and secret the provider's key lives in. The key itself is never here: it
//! does not travel in the log, does not replicate, and does not appear in a
//! backup of this table; each node reads its own vault at call time, which
//! is the whole of the design's sealing story.

use tessari_encoding::{decode_payload, encode_payload};
use tessari_types::{NamespaceId, Number, RecordId, Value};

use super::definition::object;
use super::{Catalog, system};
use crate::error::{Error, Result};

const FIELD_NAME: &str = "name";
const FIELD_NAMESPACE: &str = "namespace";
const FIELD_URL: &str = "url";
const FIELD_MODEL: &str = "model";
const FIELD_VAULT: &str = "vault";
const FIELD_SECRET: &str = "secret";
const FIELD_BUDGET: &str = "budget";
const FIELD_BUDGET_SPAN: &str = "budget_span_seconds";
const FIELD_TIMEOUT: &str = "timeout_seconds";

const ENTITY: &str = "ai provider";

/// A provider as the catalog holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AiProviderDefinition {
    /// The namespace the provider belongs to.
    pub namespace: NamespaceId,
    /// The name, unique within the namespace.
    pub name: String,
    /// The endpoint, `https` only — refused at define, re-checked here
    /// because a row on disk can be older than the rule.
    pub url: String,
    /// The model the provider is asked for.
    pub model: String,
    /// The vault the key is sealed in.
    pub vault: String,
    /// The secret's name inside that vault.
    pub secret: String,
    /// How many calls the budget admits per span.
    pub budget: u64,
    /// The budget's span, in seconds.
    pub budget_span_seconds: u64,
    /// How long one call may take, in seconds.
    pub timeout_seconds: u64,
}

impl AiProviderDefinition {
    /// The record the definition is stored under: the namespace id and the
    /// name, one key, for [`super::pool`]'s reason.
    #[must_use]
    pub fn key(namespace: NamespaceId, name: &str) -> RecordId {
        RecordId::Text(format!("{}:{name}", namespace.get()))
    }

    /// The definition as a catalog record's value.
    #[must_use]
    pub fn to_value(&self) -> Value {
        let mut fields = std::collections::BTreeMap::new();
        let whole =
            |held: u64| Value::Number(Number::Integer(i64::try_from(held).unwrap_or(i64::MAX)));
        fields.insert(FIELD_NAME.to_owned(), Value::from(self.name.as_str()));
        fields.insert(
            FIELD_NAMESPACE.to_owned(),
            Value::Number(Number::Integer(i64::from(self.namespace.get()))),
        );
        fields.insert(FIELD_URL.to_owned(), Value::from(self.url.as_str()));
        fields.insert(FIELD_MODEL.to_owned(), Value::from(self.model.as_str()));
        fields.insert(FIELD_VAULT.to_owned(), Value::from(self.vault.as_str()));
        fields.insert(FIELD_SECRET.to_owned(), Value::from(self.secret.as_str()));
        fields.insert(FIELD_BUDGET.to_owned(), whole(self.budget));
        fields.insert(
            FIELD_BUDGET_SPAN.to_owned(),
            whole(self.budget_span_seconds),
        );
        fields.insert(FIELD_TIMEOUT.to_owned(), whole(self.timeout_seconds));
        Value::Object(fields)
    }

    /// The definition a catalog record's value holds, re-checked rather than
    /// trusted: a row on disk can be older than the rules, or edited.
    ///
    /// # Errors
    ///
    /// Returns an error when a field is absent, malformed, or names a URL
    /// that is not `https`.
    pub fn from_value(value: &Value) -> Result<Self> {
        let fields = object(value, ENTITY)?;
        let malformed = |field: &'static str| Error::CatalogMalformed {
            entity: ENTITY,
            field,
            found: fields.get(field).map_or("none", Value::type_name),
        };
        let text_of = |field: &'static str| match fields.get(field) {
            Some(Value::String(held)) => Ok(held.clone()),
            _ => Err(malformed(field)),
        };
        let whole_of = |field: &'static str| match fields.get(field) {
            Some(Value::Number(Number::Integer(held))) => {
                u64::try_from(*held).map_err(|_| malformed(field))
            }
            _ => Err(malformed(field)),
        };
        let url = text_of(FIELD_URL)?;
        if !url.starts_with("https://") {
            return Err(malformed(FIELD_URL));
        }
        Ok(Self {
            namespace: NamespaceId::new(
                u32::try_from(whole_of(FIELD_NAMESPACE)?)
                    .map_err(|_| malformed(FIELD_NAMESPACE))?,
            ),
            name: text_of(FIELD_NAME)?,
            url,
            model: text_of(FIELD_MODEL)?,
            vault: text_of(FIELD_VAULT)?,
            secret: text_of(FIELD_SECRET)?,
            budget: whole_of(FIELD_BUDGET)?,
            budget_span_seconds: whole_of(FIELD_BUDGET_SPAN)?,
            timeout_seconds: whole_of(FIELD_TIMEOUT)?,
        })
    }
}

impl Catalog<'_, '_> {
    /// Write a provider under its (namespace, name) key, refused when the
    /// name is taken in that namespace.
    pub async fn create_ai_provider(&mut self, definition: &AiProviderDefinition) -> Result<()> {
        let key = system::address(
            system::AI_PROVIDERS,
            AiProviderDefinition::key(definition.namespace, &definition.name),
        );
        if self.transaction.get(&key).await?.is_some() {
            return Err(Error::NameTaken {
                holder: "an AI provider".to_owned(),
                name: definition.name.clone(),
            });
        }
        self.transaction
            .put(key, encode_payload(&definition.to_value()).into_bytes());
        Ok(())
    }

    /// Remove a provider, answering whether one was held.
    pub async fn drop_ai_provider(&mut self, namespace: NamespaceId, name: &str) -> Result<bool> {
        let key = system::address(
            system::AI_PROVIDERS,
            AiProviderDefinition::key(namespace, name),
        );
        if self.transaction.get(&key).await?.is_none() {
            return Ok(false);
        }
        self.transaction.delete(key);
        Ok(true)
    }

    /// The provider a name declares in this namespace, when one is declared.
    pub async fn ai_provider(
        &self,
        namespace: NamespaceId,
        name: &str,
    ) -> Result<Option<AiProviderDefinition>> {
        let key = system::address(
            system::AI_PROVIDERS,
            AiProviderDefinition::key(namespace, name),
        );
        match self.transaction.get(&key).await? {
            Some(payload) => Ok(Some(AiProviderDefinition::from_value(&decode_payload(
                &payload,
            )?)?)),
            None => Ok(None),
        }
    }

    /// Every provider a namespace holds.
    pub async fn ai_providers(&self, namespace: NamespaceId) -> Result<Vec<AiProviderDefinition>> {
        let mut held = Vec::new();
        for (_, payload) in self
            .transaction
            .scan_table(
                system::SYSTEM_NAMESPACE,
                system::SYSTEM_DATABASE,
                system::AI_PROVIDERS,
            )
            .await?
        {
            let definition = AiProviderDefinition::from_value(&decode_payload(&payload)?)?;
            if definition.namespace == namespace {
                held.push(definition);
            }
        }
        Ok(held)
    }
}
