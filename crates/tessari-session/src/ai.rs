//! The `ai::` call-out (G102 C8): one provider call, sealed key, bounded
//! budget, stated timeout, no retry.
//!
//! # The contract
//!
//! The provider speaks the chat-completions shape — request
//! `{"model": m, "messages": [{"role": "user", "content": prompt}]}`, answer
//! `choices[0].message.content` — because that is the one shape every hosted
//! and local provider already serves; a provider that speaks another shape
//! is a translation the caller's own endpoint can do.
//!
//! # The key
//!
//! `KEY VAULT v FIELD f` names a RECORD `f` in vault `v` whose ONE secret
//! field holds the key: the record is the secret's name, the way an operator
//! says "the anthropic_key secret in the secrets vault". The key is read at
//! call time under the engine's own authority, held in a zeroizing buffer,
//! sent in the `authorization` header, and never logged, stored or answered.
//!
//! # The budget
//!
//! A token bucket per provider, in memory, per node: `BUDGET n PER span`
//! refills `n` tokens every span, a call spends one, and an empty bucket
//! refuses `AiBudgetSpent` rather than queueing — a queued provider call
//! inside a read is a stalled read, and the caller's retry is the honest
//! queue.

use std::time::Duration;

use tessari_ql::{Function, Span};
use tessari_storage::{AiProviderDefinition, Catalog, RecordAddress, Transaction};
use tessari_types::Value;
use zeroize::Zeroizing;

use crate::error::{Error, Result};
use crate::session::Session;

/// The most bytes an answer may hold — the frame ceiling's rule, applied to
/// the one surface whose far end is not this store.
const ANSWER_MOST: usize = 1 << 20;

impl Session<'_> {
    /// The `ai::` family — one provider call, shaped per function.
    ///
    /// `ai::generate` sends its prompt as written; the derived four wrap
    /// their arguments in the instruction their name promises, and
    /// `ai::classify` validates the reply against the labels the call
    /// declared, answering `NONE` for a reply that names none of them.
    ///
    /// # Errors
    ///
    /// [`Error::AiNoProvider`] when the namespace declares none (or several
    /// and the call cannot choose), [`Error::AiKeyNotSealed`] when the key
    /// cannot be read, [`Error::AiBudgetSpent`], [`Error::AiTimedOut`],
    /// [`Error::AiProviderRefused`], [`Error::AiAnswerTooLarge`].
    pub(crate) async fn ai(
        &self,
        transaction: &mut Transaction<'_>,
        function: Function,
        arguments: &[tessari_ql::Expr],
        scope: crate::evaluate::Scope<'_>,
        span: Span,
    ) -> Result<Value> {
        let mut values = Vec::with_capacity(arguments.len());
        for argument in arguments {
            values.push(self.evaluate_in(transaction, argument, scope).await?);
        }
        let Some(prompt) = prompt_of(function, &values) else {
            return Ok(Value::None);
        };
        let answer = self.ai_call(transaction, &prompt, span).await?;
        Ok(match function {
            Function::AiClassify => classified(&answer, &values),
            _ => Value::String(answer),
        })
    }

    /// One provider call — the provider the namespace declares, its budget,
    /// its key and the transport: everything the family shares.
    async fn ai_call(
        &self,
        transaction: &mut Transaction<'_>,
        prompt: &str,
        span: Span,
    ) -> Result<String> {
        let context = self.context(transaction, None, span).await?;
        let providers = Catalog::new(transaction)
            .ai_providers(context.namespace)
            .await?;
        let provider = match providers.as_slice() {
            [] => {
                return Err(Error::AiNoProvider {
                    reason: "this namespace declares none",
                    span,
                });
            }
            [only] => only.clone(),
            _ => {
                return Err(Error::AiNoProvider {
                    reason: "this namespace declares several, and the call names none",
                    span,
                });
            }
        };
        if !self.store.ai_budgets().spend(
            &provider.name,
            provider.budget,
            Duration::from_secs(provider.budget_span_seconds),
        ) {
            return Err(Error::AiBudgetSpent {
                provider: provider.name.clone(),
                span,
            });
        }
        let key = self.ai_key(transaction, &provider, span).await?;
        transport::post(
            &provider,
            &key,
            prompt,
            Duration::from_secs(provider.timeout_seconds),
            self.ai_roots.as_deref(),
        )
        .await
    }

    /// The provider's key, opened from its vault at call time.
    async fn ai_key(
        &self,
        transaction: &mut Transaction<'_>,
        provider: &AiProviderDefinition,
        span: Span,
    ) -> Result<Zeroizing<String>> {
        let sealed = |reason: &'static str| Error::AiKeyNotSealed {
            provider: provider.name.clone(),
            reason,
            span,
        };
        let context = self.context(transaction, None, span).await?;
        let id = Catalog::new(transaction)
            .table_id(context.namespace, context.database, &provider.vault)
            .await?
            .ok_or_else(|| sealed("names a vault this database no longer holds"))?;
        let definition = Catalog::new(transaction)
            .table(id)
            .await?
            .filter(|held| held.is_vault())
            .ok_or_else(|| sealed("names a table that is not a vault"))?;
        // The FIELD clause names the record; the record's one secret field
        // holds the key — the record is the secret's name.
        let secrets: Vec<String> = Catalog::new(transaction)
            .declared_fields(id)
            .await?
            .into_iter()
            .filter(|field| field.secret)
            .map(|field| field.name)
            .collect();
        let address = RecordAddress::new(
            context.namespace,
            context.database,
            id,
            tessari_types::RecordId::Text(provider.secret.clone()),
        );
        let stored = transaction
            .get(&address)
            .await?
            .ok_or_else(|| sealed("names a secret the vault does not hold"))?;
        let Value::Object(held) = tessari_encoding::decode_payload(&stored)? else {
            return Err(sealed("names a record that is not a secret"));
        };
        let data_key = tessari_storage::open_data_key(transaction, &address, &definition, &held)
            .map_err(|_| sealed("is sealed — unseal the vault to call out"))?;
        let mut opened = None;
        for name in &secrets {
            if let Some(Value::Bytes(envelope)) = held.get(name) {
                let value =
                    tessari_storage::open_field(&data_key, &address, &definition, name, envelope)
                        .map_err(|_| sealed("is sealed — unseal the vault to call out"))?;
                if let Value::String(text) = value {
                    opened = Some(Zeroizing::new(text));
                    break;
                }
            }
        }
        opened.ok_or_else(|| sealed("holds no text secret to send"))
    }
}

/// The prompt a function's arguments spell, or `None` when an argument is
/// absent or of a kind the call has no meaning for — absence narrows, so a
/// row without the text is a row the call skips, not a call that fails.
fn prompt_of(function: Function, values: &[Value]) -> Option<String> {
    let text = |at: usize| match values.get(at) {
        Some(Value::String(held)) => Some(held.as_str()),
        _ => None,
    };
    match function {
        Function::AiGenerate => text(0).map(str::to_owned),
        Function::AiSummarize => {
            text(0).map(|held| format!("Summarize this text in a few sentences:\n\n{held}"))
        }
        Function::AiClassify => {
            let held = text(0)?;
            let Some(Value::Array(declared)) = values.get(1) else {
                return None;
            };
            let labels: Option<Vec<&str>> = declared
                .iter()
                .map(|label| match label {
                    Value::String(held) => Some(held.as_str()),
                    _ => None,
                })
                .collect();
            let labels = labels?;
            if labels.is_empty() {
                return None;
            }
            Some(format!(
                "Classify the text into exactly one of these labels: {}. \
                 Answer with the label alone.\n\n{held}",
                labels.join(", "),
            ))
        }
        Function::AiExtract => {
            let held = text(0)?;
            let field = text(1)?;
            Some(format!(
                "Extract the value of \"{field}\" from the text. \
                 Answer with the value alone.\n\n{held}"
            ))
        }
        Function::AiTranslate => {
            let held = text(0)?;
            let language = text(1)?;
            Some(format!(
                "Translate the following text into {language}. \
                 Answer with the translation alone.\n\n{held}"
            ))
        }
        _ => None,
    }
}

/// The label a classify reply names, when it names one of the declared ones
/// — answered in the DECLARED spelling, so a reply's stray whitespace cannot
/// mint a label the statement did not write, and a reply naming none of them
/// is `NONE` rather than a label the call never offered.
fn classified(answer: &str, values: &[Value]) -> Value {
    let Some(Value::Array(declared)) = values.get(1) else {
        return Value::None;
    };
    let reply = answer.trim();
    declared
        .iter()
        .find_map(|label| match label {
            Value::String(held) if held.as_str() == reply => Some(Value::String(held.clone())),
            _ => None,
        })
        .unwrap_or(Value::None)
}

/// The outbound half: one HTTPS POST, the answer's text out of it.
pub(crate) mod transport {
    use super::{ANSWER_MOST, AiProviderDefinition, Duration, Error, Result, Zeroizing};

    /// One call. `roots` is the test seam: `None` trusts the public roots,
    /// which is the only authority a public provider can have; a test hands
    /// its own so a stub server with a test CA answers.
    pub(crate) async fn post(
        provider: &AiProviderDefinition,
        key: &Zeroizing<String>,
        prompt: &str,
        timeout: Duration,
        roots: Option<&rustls::RootCertStore>,
    ) -> Result<String> {
        tokio::time::timeout(timeout, exchange(provider, key, prompt, roots))
            .await
            .map_err(|_| Error::AiTimedOut {
                provider: provider.name.clone(),
                seconds: timeout.as_secs(),
                span: tessari_ql::Span::new(0, 0),
            })?
    }

    async fn exchange(
        provider: &AiProviderDefinition,
        key: &Zeroizing<String>,
        prompt: &str,
        roots: Option<&rustls::RootCertStore>,
    ) -> Result<String> {
        let unreachable = |detail: &'static str| Error::AiProviderRefused {
            provider: provider.name.clone(),
            status: detail,
            span: tessari_ql::Span::new(0, 0),
        };
        let parts = crate::web::url_parts(&provider.url).ok_or_else(|| unreachable("5xx"))?;
        let host = parts.host.clone().ok_or_else(|| unreachable("5xx"))?;
        let port = parts.port.unwrap_or(443);
        let mut tls_roots = rustls::RootCertStore::empty();
        match roots {
            Some(held) => tls_roots.extend(held.roots.iter().cloned()),
            None => tls_roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned()),
        }
        let config = rustls::ClientConfig::builder()
            .with_root_certificates(tls_roots)
            .with_no_client_auth();
        let server_name = rustls::pki_types::ServerName::try_from(host.to_owned())
            .map_err(|_| unreachable("5xx"))?;
        let tcp = tokio::net::TcpStream::connect((host.as_str(), port))
            .await
            .map_err(|_| unreachable("5xx"))?;
        let mut stream = tokio_rustls::TlsConnector::from(std::sync::Arc::new(config))
            .connect(server_name, tcp)
            .await
            .map_err(|_| unreachable("5xx"))?;
        // The request: the chat-completions shape, the key in the one header
        // that carries it, `Connection: close` so the answer's end is the
        // stream's end and no chunk framing is guessed at.
        let body = request_body(&provider.model, prompt);
        let path = if parts.path.is_empty() {
            "/"
        } else {
            &parts.path
        };
        let request = format!(
            "POST {path} HTTP/1.1\r\nhost: {host}\r\nauthorization: Bearer {}\r\n\
             content-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            key.as_str(),
            body.len(),
        );
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        stream
            .write_all(request.as_bytes())
            .await
            .map_err(|_| unreachable("5xx"))?;
        let mut received = Vec::new();
        let mut buffer = [0_u8; 8192];
        loop {
            // A close without `close_notify` ends the stream like any other:
            // the request asked for `connection: close`, so the answer's end
            // IS the stream's end, and rustls 0.23's unexpected-EOF error
            // would otherwise fail every call to a provider that skips the
            // courtesy. The bytes read so far are the whole answer either
            // way; a body cut mid-way fails the shape check in `answer_of`.
            let read = match stream.read(&mut buffer).await {
                Ok(read) => read,
                Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => break,
                Err(_) => return Err(unreachable("5xx")),
            };
            if read == 0 {
                break;
            }
            received.extend_from_slice(buffer.get(..read).unwrap_or_default());
            if received.len() > ANSWER_MOST.saturating_add(16 * 1024) {
                return Err(Error::AiAnswerTooLarge {
                    provider: provider.name.clone(),
                    span: tessari_ql::Span::new(0, 0),
                });
            }
        }
        answer_of(provider, &received)
    }

    /// `{"model": m, "messages": [{"role": "user", "content": prompt}]}`,
    /// with the two strings JSON-escaped.
    fn request_body(model: &str, prompt: &str) -> String {
        let mut out = String::with_capacity(prompt.len().saturating_add(96));
        out.push_str("{\"model\":");
        escape_into(&mut out, model);
        out.push_str(",\"messages\":[{\"role\":\"user\",\"content\":");
        escape_into(&mut out, prompt);
        out.push_str("}]}");
        out
    }

    /// A JSON string, quotes included.
    fn escape_into(out: &mut String, text: &str) {
        out.push('"');
        for character in text.chars() {
            match character {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                held if held.is_control() => {
                    out.push_str(&format!("\\u{:04x}", u32::from(held)));
                }
                held => out.push(held),
            }
        }
        out.push('"');
    }

    /// The status and the answer text out of the received bytes.
    fn answer_of(provider: &AiProviderDefinition, received: &[u8]) -> Result<String> {
        let text = String::from_utf8_lossy(received);
        let (head, body) = text.split_once("\r\n\r\n").unwrap_or((text.as_ref(), ""));
        let status: u32 = head
            .split_whitespace()
            .nth(1)
            .and_then(|held| held.parse().ok())
            .unwrap_or(0);
        if !(200..300).contains(&status) {
            // The class, never the body and never the key: a provider's error
            // page can echo a request, and the request carried the key.
            return Err(Error::AiProviderRefused {
                provider: provider.name.clone(),
                status: match status {
                    400..=499 => "4xx",
                    _ => "5xx",
                },
                span: tessari_ql::Span::new(0, 0),
            });
        }
        if body.len() > ANSWER_MOST {
            return Err(Error::AiAnswerTooLarge {
                provider: provider.name.clone(),
                span: tessari_ql::Span::new(0, 0),
            });
        }
        // `choices[0].message.content`, read by a walk rather than a JSON
        // dependency: the session crate parses JSON nowhere else, and the
        // shape is one path deep.
        let content = json_string_after(body, "\"content\"").ok_or(Error::AiProviderRefused {
            provider: provider.name.clone(),
            status: "2xx without a chat-completions body",
            span: tessari_ql::Span::new(0, 0),
        })?;
        Ok(content)
    }

    /// The first JSON string value following `"key":`, unescaped.
    fn json_string_after(body: &str, key: &str) -> Option<String> {
        let at = body.find(key)?;
        let after = body.get(at.saturating_add(key.len())..)?;
        let after = after.trim_start();
        let after = after.strip_prefix(':')?.trim_start();
        let mut chars = after.strip_prefix('"')?.chars();
        let mut out = String::new();
        while let Some(held) = chars.next() {
            match held {
                '"' => return Some(out),
                '\\' => match chars.next()? {
                    'n' => out.push('\n'),
                    'r' => out.push('\r'),
                    't' => out.push('\t'),
                    '"' => out.push('"'),
                    '\\' => out.push('\\'),
                    '/' => out.push('/'),
                    'u' => {
                        let code: String = chars.by_ref().take(4).collect();
                        let value = u32::from_str_radix(&code, 16).ok()?;
                        out.push(char::from_u32(value)?);
                    }
                    _ => return None,
                },
                held => out.push(held),
            }
        }
        None
    }
}
