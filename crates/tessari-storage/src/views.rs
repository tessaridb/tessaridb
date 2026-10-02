//! What a materialized view keeps beside its rows (ADR-0109).
//!
//! One system row per view, written in the maintaining transaction with the
//! rows it describes so the two can never disagree: the version the rows equal
//! the view's read at, each log's position the maintainer has reached, and when
//! it last reached the store's head.
//!
//! It lives in the system tenancy, so it never appears in the change feed a
//! maintainer reads, and it travels in the log like any catalog row: a follower
//! holds the rows and the state its leader wrote.

use std::collections::BTreeMap;

use tessari_encoding::{AppliedPositionKey, LogId, StoreKey, decode_payload, encode_payload};
use tessari_types::{DatabaseId, NamespaceId, Number, Reach, RecordId, Sequence, TableId, Value};

use crate::catalog::system;
use crate::error::{Error, Result};
use crate::store::Store;
use crate::transaction::Transaction;

const ENTITY: &str = "view state";
const FIELD_VERSION: &str = "version";
const FIELD_POSITIONS: &str = "positions";
const FIELD_REFRESHED: &str = "refreshed";
const FIELD_LOG: &str = "log";
const FIELD_AT: &str = "at";

/// How far a materialized view has been brought.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewState {
    /// The version the view's rows equal its read at.
    pub version: Sequence,
    /// Where the maintainer reads each log from next.
    pub positions: Vec<(LogId, Sequence)>,
    /// When the view last reached the store's head, as milliseconds since the
    /// Unix epoch.
    pub refreshed: i64,
}

impl ViewState {
    fn to_value(&self) -> Value {
        let positions = self
            .positions
            .iter()
            .map(|(log, at)| {
                Value::Object(BTreeMap::from([
                    (
                        FIELD_LOG.to_owned(),
                        Value::Bytes(AppliedPositionKey::new(*log).encode().as_slice().to_vec()),
                    ),
                    (FIELD_AT.to_owned(), count(at.get())),
                ]))
            })
            .collect();
        Value::Object(BTreeMap::from([
            (FIELD_VERSION.to_owned(), count(self.version.get())),
            (FIELD_POSITIONS.to_owned(), Value::Array(positions)),
            (
                FIELD_REFRESHED.to_owned(),
                Value::Number(Number::Integer(self.refreshed)),
            ),
        ]))
    }

    fn from_value(value: &Value) -> Result<Self> {
        let Value::Object(fields) = value else {
            return Err(malformed("state", value));
        };
        let version = Sequence::new(unsigned(fields.get(FIELD_VERSION), FIELD_VERSION)?);
        let Some(Value::Array(held)) = fields.get(FIELD_POSITIONS) else {
            return Err(malformed_field(
                FIELD_POSITIONS,
                fields.get(FIELD_POSITIONS),
            ));
        };
        let mut positions = Vec::with_capacity(held.len());
        for one in held {
            let Value::Object(position) = one else {
                return Err(malformed("position", one));
            };
            let Some(Value::Bytes(log)) = position.get(FIELD_LOG) else {
                return Err(malformed_field(FIELD_LOG, position.get(FIELD_LOG)));
            };
            let log = AppliedPositionKey::decode(log)?.log;
            let at = Sequence::new(unsigned(position.get(FIELD_AT), FIELD_AT)?);
            positions.push((log, at));
        }
        let refreshed = match fields.get(FIELD_REFRESHED) {
            Some(Value::Number(Number::Integer(at))) => *at,
            other => return Err(malformed_field(FIELD_REFRESHED, other)),
        };
        Ok(Self {
            version,
            positions,
            refreshed,
        })
    }
}

fn count(held: u64) -> Value {
    Value::Number(Number::Integer(i64::try_from(held).unwrap_or(i64::MAX)))
}

fn unsigned(value: Option<&Value>, field: &'static str) -> Result<u64> {
    match value {
        Some(Value::Number(Number::Integer(held))) => {
            u64::try_from(*held).map_err(|_| malformed_field(field, value))
        }
        other => Err(malformed_field(field, other)),
    }
}

fn malformed(field: &'static str, found: &Value) -> Error {
    Error::CatalogMalformed {
        entity: ENTITY,
        field,
        found: found.type_name(),
    }
}

fn malformed_field(field: &'static str, found: Option<&Value>) -> Error {
    Error::CatalogMalformed {
        entity: ENTITY,
        field,
        found: found.map_or("none", Value::type_name),
    }
}

fn state_id(view: TableId) -> RecordId {
    RecordId::Int(i64::from(view.get()))
}

impl Transaction<'_> {
    /// How far a materialized view has been brought, if it has a state.
    ///
    /// # Errors
    ///
    /// Returns an error when the store cannot be read or the state is
    /// malformed.
    pub fn view_state(&self, view: TableId) -> Result<Option<ViewState>> {
        let address = system::address(system::VIEW_STATES, state_id(view));
        match self.get(&address)? {
            Some(bytes) => Ok(Some(ViewState::from_value(&decode_payload(&bytes)?)?)),
            None => Ok(None),
        }
    }

    /// Record how far a materialized view has been brought.
    ///
    /// # Errors
    ///
    /// Returns an error when the state cannot be encoded.
    pub fn put_view_state(&mut self, view: TableId, state: &ViewState) -> Result<()> {
        let payload = encode_payload(&state.to_value()).into_bytes();
        self.put(
            system::address(system::VIEW_STATES, state_id(view)),
            payload,
        );
        Ok(())
    }

    /// Remove the state kept beside a materialized view's rows.
    pub fn forget_view(&mut self, view: TableId) {
        self.delete(system::address(system::VIEW_STATES, state_id(view)));
    }
}

impl Store {
    /// Every log this node writes that can carry a change to one table: the
    /// store's, the namespace's, the database's and each of the table's shards'.
    ///
    /// A commit is filed in the log of the narrowest home covering all it
    /// touches, so a change to a table may sit in any of these — a write beside
    /// a definition is filed in the store's. Another writer's logs are left
    /// out: their order says nothing about this one's (`feed::Merged`).
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails or this store has no identity.
    pub fn logs_carrying(
        &self,
        namespace: NamespaceId,
        database: DatabaseId,
        table: TableId,
    ) -> Result<Vec<LogId>> {
        let own = self.writer()?;
        Ok(self
            .logs()?
            .into_iter()
            .filter(|log| log.writer == own)
            .filter(|log| match log.home {
                Reach::Store => true,
                Reach::Namespace(held) => held == namespace,
                Reach::Database(held, within) => held == namespace && within == database,
                Reach::Shard(held, within, sharded, _) => {
                    held == namespace && within == database && sharded == table
                }
            })
            .collect())
    }

    /// The first position of `log` holding a commit ordered after `version` —
    /// where a reader that already reflects `version` starts.
    ///
    /// Read from the tail, because a reader asks this about a recent version
    /// and the answer is a few records back. A record that carries no order was
    /// written before commits carried one and is taken as already reflected.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails or a record cannot be decoded.
    pub fn first_after(&self, log: LogId, version: Sequence) -> Result<Sequence> {
        let mut next = self.committed_tail(log)?.get().saturating_add(1);
        let mut window = 8_usize;
        loop {
            let newest = self.log_records_newest_first(log, window)?;
            let read = newest.len();
            for (at, record) in &newest {
                if record.order().is_none_or(|order| order <= version) {
                    return Ok(Sequence::new(next));
                }
                next = at.get();
            }
            if read < window {
                return Ok(Sequence::new(next));
            }
            window = window.saturating_mul(2);
        }
    }
}
