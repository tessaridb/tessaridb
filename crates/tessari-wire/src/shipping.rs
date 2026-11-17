//! The shipment: one link's batch on the client wire, and the sink that sends it.
//!
//! A cross-cluster link's batch is an ordinary client-door citizen: the door at
//! 9080 is where the other cluster answers, and the pair's token — not a member
//! certificate, which is what the peer door judges and this cluster cannot
//! present to a cluster it does not belong to — is the admission rule. So the
//! shipment rides a new frame kind beside the request and the vault ask: the
//! [`LinkAsk`] names the database in the remote's spelling and presents the
//! token, and the answer names the position the far side has durably applied
//! through, which is the confirmation the at-least-once boundary is drawn at.
//!
//! # The token is configured, never declared
//!
//! The receiving cluster creates the token on its inbound row (`CREATE LINK
//! TOKEN FOR REPLICATION`), keeps the digest and the expiry there, and answers
//! the hex once. The operator carries it to the shipping cluster's environment
//! — `TESSARIDB_LINK_TOKEN_<NAME>`, the join token's precedent: a secret enters
//! through the environment, and the catalog in every backup holds nothing that
//! binds a link by itself. The source presents the token in the ask; the
//! receiving cluster verifies its digest against the row before applying.

use std::future::Future;

use tessari_encoding::{NODE_ID_LEN, StoreValue};
use tessari_storage::{Catalog, Envelope, Reach, Sink, Store, intake};
use tessari_types::{RefusalClass, Sequence};

use crate::error::{Error, Result};
use crate::{frame, frame_async};

/// What a link's shipment asks: the pair's token, the database in the remote's
/// spelling, and one envelope read from the source's log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LinkAsk {
    /// The token the receiving cluster's operator carried here — offered inside
    /// the channel the operator chose for the door, exactly where a client
    /// credential rides.
    pub(crate) token: [u8; 32],
    /// The namespace's name as the receiving cluster holds it.
    pub(crate) namespace: String,
    /// The database's name as the receiving cluster holds it — the `AS` remote
    /// spelling when the link said one, the same name otherwise.
    pub(crate) database: String,
    /// One batch, read from one log at the positions the far side has not
    /// confirmed.
    pub(crate) envelope: Envelope,
}

impl LinkAsk {
    /// The body of a [`frame::Kind::Link`] frame.
    #[must_use]
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut body = Vec::new();
        frame::put_bytes(&mut body, &self.token);
        frame::put_text(&mut body, &self.namespace);
        frame::put_text(&mut body, &self.database);
        body.extend_from_slice(&encode_envelope(&self.envelope));
        body
    }

    /// Read one back.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Malformed`] when the body is not the shape an ask takes.
    pub(crate) fn decode(body: &[u8]) -> Result<Self> {
        let (token, at) = frame::take_bytes(body, 0)?;
        let Some(token) = <[u8; 32]>::try_from(token).ok() else {
            return Err(Error::Malformed);
        };
        let (namespace, at) = frame::take_text(body, at)?;
        let (database, at) = frame::take_text(body, at)?;
        let (envelope, _) = decode_envelope_at(body, at)?;
        Ok(Self {
            token,
            namespace,
            database,
            envelope,
        })
    }
}

/// What a link's shipment is answered with: the position the far side has
/// durably applied through — the confirmation the source's registry advances on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LinkAnswer {
    pub(crate) confirmed: Sequence,
}

impl LinkAnswer {
    /// The body of the [`frame::Kind::Link`] frame that answers an ask.
    #[must_use]
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut body = Vec::new();
        frame::put_u64(&mut body, self.confirmed.get());
        body
    }

    /// Read one back.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Malformed`] when the body is not the shape an answer takes.
    pub(crate) fn decode(body: &[u8]) -> Result<Self> {
        let (confirmed, _) = frame::take_u64(body, 0)?;
        Ok(Self {
            confirmed: Sequence::new(confirmed),
        })
    }
}

/// One envelope, in the shape it rides the wire: the origin the far side bound
/// the token to, the log the positions count in, and the records in the store's
/// own encoding — a second codec for the same record is a second thing that has
/// to stay true across a version, which is why the records carry
/// [`tessari_encoding::LogRecord::encode`] unchanged.
#[must_use]
pub(crate) fn encode_envelope(envelope: &Envelope) -> Vec<u8> {
    let mut body = Vec::new();
    frame::put_bytes(&mut body, &envelope.origin);
    frame::put_log(&mut body, envelope.log);
    frame::put_u64(&mut body, envelope.from.get());
    frame::put_u64(
        &mut body,
        u64::try_from(envelope.records.len()).unwrap_or(u64::MAX),
    );
    for (at, record) in &envelope.records {
        frame::put_u64(&mut body, at.get());
        frame::put_bytes(&mut body, record.encode().as_slice());
    }
    body
}

/// Read one envelope back, from the start of `body`.
///
/// # Errors
///
/// Returns [`Error::Malformed`] when the body stops before the envelope does,
/// or a record cannot be decoded.
#[cfg(test)]
pub(crate) fn decode_envelope(body: &[u8]) -> Result<Envelope> {
    decode_envelope_at(body, 0).map(|(envelope, _)| envelope)
}

fn decode_envelope_at(body: &[u8], at: usize) -> Result<(Envelope, usize)> {
    let (origin, at) = frame::take_bytes(body, at)?;
    let Some(origin) = <[u8; NODE_ID_LEN]>::try_from(origin).ok() else {
        return Err(Error::Malformed);
    };
    let (log, at) = frame::take_log(body, at)?;
    let (from, at) = frame::take_u64(body, at)?;
    let (count, mut at) = frame::take_u64(body, at)?;
    // Deliberately not `with_capacity(count)`: the count came from the other
    // end, and a reader that allocated whatever it was told would be one frame
    // away from being out of memory. The body is already bounded by the frame
    // ceiling, so growing as records actually arrive costs nothing that matters
    // and cannot be driven.
    let mut records = Vec::new();
    for _ in 0..count {
        let (position, next) = frame::take_u64(body, at)?;
        let (record, next) = frame::take_bytes(body, next)?;
        records.push((
            Sequence::new(position),
            tessari_encoding::LogRecord::decode(&record)?,
        ));
        at = next;
    }
    Ok((
        Envelope {
            origin,
            log,
            from: Sequence::new(from),
            records,
        },
        at,
    ))
}

/// The variable a link's token is read from: the join token's environment
/// precedent, one variable per link, named by the link in upper case with
/// everything that is not a letter or a digit read as an underscore.
fn token_variable(link: &str) -> String {
    let mut named = String::from("TESSARIDB_LINK_TOKEN_");
    for letter in link.chars() {
        if letter.is_ascii_alphanumeric() {
            named.push(letter.to_ascii_uppercase());
        } else {
            named.push('_');
        }
    }
    named
}

/// A [`Sink`] that ships each batch to the link's endpoint over the client
/// wire. Constructed once by the node that serves links; the loop paces it.
#[derive(Debug, Clone)]
pub struct Shipping {
    store: Store,
}

impl Shipping {
    /// A sink that reads its links' rows from `store` and their tokens from the
    /// environment.
    #[must_use]
    pub fn new(store: Store) -> Self {
        Self { store }
    }

    /// The token one link presents, from its environment variable, refused with
    /// the variable's name when the operator has not set it.
    fn token(link: &str) -> Result<[u8; 32]> {
        let named = token_variable(link);
        let written = std::env::var(&named).map_err(|_| {
            Error::Io(std::io::Error::other(format!(
                "{named} holds the token CREATE LINK TOKEN answered for link '{link}'"
            )))
        })?;
        if written.len() != 64 {
            return Err(Error::Io(std::io::Error::other(format!(
                "{named} is the 64 hexadecimal digits the token statement answered"
            ))));
        }
        let mut token = [0_u8; 32];
        for (slot, pair) in token.iter_mut().zip(written.as_bytes().chunks(2)) {
            let pair = std::str::from_utf8(pair).map_err(|_| Error::Malformed)?;
            *slot = u8::from_str_radix(pair, 16).map_err(|_| Error::Malformed)?;
        }
        Ok(token)
    }
}

impl Sink for Shipping {
    fn ship(
        &mut self,
        link: &str,
        envelope: Envelope,
    ) -> impl Future<Output = tessari_storage::Result<Sequence>> + Send {
        let store = self.store.clone();
        let link = link.to_owned();
        async move {
            ship(&store, &link, envelope)
                .await
                .map_err(|why| tessari_storage::Error::LinkRefused {
                    reason: match why {
                        Error::Refused { message, .. } => message,
                        other => other.to_string(),
                    },
                })
        }
    }
}

/// One pass of one link over the wire: read the row fresh (the endpoint may
/// have moved since the loop last looked), present the token, ship, and answer
/// the confirmed position.
async fn ship(store: &Store, link: &str, envelope: Envelope) -> Result<Sequence> {
    let (endpoint, ask) = {
        let mut reading = store
            .begin()
            .await
            .map_err(|why| Error::Io(std::io::Error::other(why.to_string())))?;
        let catalog = Catalog::new(&mut reading);
        let definition = catalog
            .replications()
            .await
            .map_err(|why| Error::Io(std::io::Error::other(why.to_string())))?
            .into_iter()
            .find(|held| held.name == link)
            .ok_or_else(|| {
                Error::Io(std::io::Error::other(format!(
                    "link '{link}' is not declared"
                )))
            })?;
        let Reach::Database(namespace, database) = definition.database else {
            return Err(Error::Io(std::io::Error::other(format!(
                "link '{link}' does not name a database"
            ))));
        };
        let namespaces = catalog
            .namespaces()
            .await
            .map_err(|why| Error::Io(std::io::Error::other(why.to_string())))?;
        let Some(held) = namespaces.iter().find(|held| held.id == namespace) else {
            return Err(Error::Io(std::io::Error::other(format!(
                "link '{link}' names a namespace the catalog has lost"
            ))));
        };
        let namespace_name = held.name.clone();
        let databases = catalog
            .databases_in(namespace)
            .await
            .map_err(|why| Error::Io(std::io::Error::other(why.to_string())))?;
        let Some(held) = databases.iter().find(|held| held.id == database) else {
            return Err(Error::Io(std::io::Error::other(format!(
                "link '{link}' names a database the catalog has lost"
            ))));
        };
        let database_name = held.name.clone();
        let ask = LinkAsk {
            token: Shipping::token(link)?,
            namespace: namespace_name,
            database: definition.remote.clone().unwrap_or(database_name),
            envelope,
        };
        (definition.endpoint, ask)
    };
    let stream = tokio::net::TcpStream::connect(endpoint.as_str())
        .await
        .map_err(|why| Error::Io(why))?;
    if let Err(why) = stream.set_nodelay(true) {
        tracing::warn!(error = %why, "a link shipment could not turn off Nagle's algorithm");
    }
    let (mut reader, mut writer) = tokio::io::split(stream);
    let theirs = frame_async::greet(&mut reader, &mut writer).await?;
    if theirs < frame::LINKED {
        return Err(Error::NodeTooOld {
            found: theirs,
            needed: frame::LINKED,
        });
    }
    frame_async::write(&mut writer, frame::Kind::Link, &ask.encode()).await?;
    match frame_async::read(&mut reader).await? {
        Some((frame::Kind::Link, body)) => Ok(LinkAnswer::decode(&body)?.confirmed),
        Some((frame::Kind::Refusal, body)) => {
            let (class, message) = frame::read_refusal(&body);
            Err(Error::Refused { message, class })
        }
        Some((other, _)) => Err(Error::UnknownFrame { tag: other.tag() }),
        None => Err(Error::Truncated),
    }
}

/// The receiving half of the shipment, run at the door a link's batch arrives
/// at: resolve the spelled database to this cluster's tenancy, find the inbound
/// link over it, verify the presented token against the row's digest and
/// expiry, and apply the envelope through the follower's intake — answering the
/// position applied through, which is the confirmation the source draws its
/// at-least-once boundary at.
///
/// The refusal classes: `Invalid` names a shipment that does not describe a
/// database this cluster receives (repeating it unchanged cannot succeed),
/// `Unauthenticated` names a token that is absent, expired, or not the row's,
/// and `Conflict` names an apply the copy cannot take — a gap where a position
/// was pruned, or a record that diverges from what the copy holds.
pub(crate) async fn receive_link(
    db: &tessaridb::Db,
    asked: LinkAsk,
) -> std::result::Result<Sequence, (RefusalClass, String)> {
    let store = db.store();
    let row = {
        let mut reading = store
            .begin()
            .await
            .map_err(|why| (RefusalClass::Internal, why.to_string()))?;
        let catalog = Catalog::new(&mut reading);
        let namespaces = catalog
            .namespaces()
            .await
            .map_err(|why| (RefusalClass::Internal, why.to_string()))?;
        let Some(held) = namespaces.iter().find(|held| held.name == asked.namespace) else {
            return Err((
                RefusalClass::Invalid,
                format!("this cluster holds no namespace '{}'", asked.namespace),
            ));
        };
        let namespace = held.id;
        let databases = catalog
            .databases_in(namespace)
            .await
            .map_err(|why| (RefusalClass::Internal, why.to_string()))?;
        let Some(held) = databases.iter().find(|held| held.name == asked.database) else {
            return Err((
                RefusalClass::Invalid,
                format!(
                    "this cluster holds no database '{}.{}'",
                    asked.namespace, asked.database
                ),
            ));
        };
        let tenancy = (namespace, held.id);
        let Some(link) = store.replicated().link_over(tenancy) else {
            return Err((
                RefusalClass::Invalid,
                format!(
                    "'{}.{}' is not received from another cluster",
                    asked.namespace, asked.database
                ),
            ));
        };
        let rows = catalog
            .replications()
            .await
            .map_err(|why| (RefusalClass::Internal, why.to_string()))?;
        let Some(row) = rows.into_iter().find(|held| held.name == link) else {
            return Err((
                RefusalClass::Internal,
                format!("the registry named link '{link}' the catalog does not hold"),
            ));
        };
        row
    };
    let ticket = row.join.ok_or_else(|| {
        (
            RefusalClass::Unauthenticated,
            format!("the link '{}' waits on no token", row.name),
        )
    })?;
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| {
            i64::try_from(since.as_millis()).unwrap_or(i64::MAX)
        });
    if now_ms > ticket.expires_ms {
        return Err((
            RefusalClass::Unauthenticated,
            format!(
                "the link '{}' token has expired; create a new one",
                row.name
            ),
        ));
    }
    let offered: String = <sha2::Sha256 as sha2::Digest>::digest(asked.token)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    if offered != ticket.digest {
        return Err((
            RefusalClass::Unauthenticated,
            format!("the offered token is not the link '{}' token", row.name),
        ));
    }
    intake(store, asked.envelope)
        .await
        .map_err(|why| match why {
            tessari_storage::Error::LogGap { .. }
            | tessari_storage::Error::LogDivergence { .. } => {
                (RefusalClass::Conflict, why.to_string())
            }
            other => (RefusalClass::Internal, other.to_string()),
        })
}

#[cfg(test)]
mod tests;
