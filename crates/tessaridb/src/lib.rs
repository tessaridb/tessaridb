//! TessariDB, embedded.
//!
//! One type to open a database, one to run a script against it, and the
//! vocabulary those two speak in. Everything else in this workspace is how the
//! store is built rather than how it is used, and is deliberately not reachable
//! from here.
//!
//! ```
//! use tessaridb::{Db, Value};
//!
//! let db = Db::in_memory()?;
//! let mut session = db.session();
//! session.run(
//!     "DEFINE NAMESPACE prod;
//!      USE NAMESPACE prod;
//!      DEFINE DATABASE orders;
//!      USE DATABASE orders;
//!      DEFINE COLLECTION users;
//!      CREATE users:1 = { name: 'ada' };",
//! )?;
//!
//! let found = session.run("SELECT name FROM users:1;")?;
//! let records = found[0].records().expect("a read answers with records");
//! assert_eq!(records.len(), 1);
//! # Ok::<(), tessaridb::Error>(())
//! ```
//!
//! # Two ways to open, one behaviour
//!
//! [`Db::in_memory`] and [`Db::open`] differ in where the bytes live and in
//! nothing else. That is not a convenience — it is the claim the whole storage
//! layer is built to support, and the conformance suite that runs against both
//! substrates is what makes it a claim rather than a hope.
//!
//! # Following what changes
//!
//! The change feed is a projection of the replication log rather than a
//! mechanism of its own, so it needs no setup and holds no state: a
//! [`Subscription`] is a value you keep, and its position is a number you can
//! store and come back with. See [`Db::changes_since`].

#![forbid(unsafe_code)]
// `expect_used` and `as_conversions` govern production code; a test states its own expectations.
#![cfg_attr(test, allow(clippy::expect_used, clippy::as_conversions))]

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_lsm::LsmBackend;
use tessari_storage::{Catalog, ReplicaDefinition, Roles, Store};
use tessari_types::ShardId;

mod across;
mod balance;
mod changes;
mod coordinate;
mod db;
pub mod feed;
mod leadership;
mod naming;
mod opening;
mod values;
mod wiring;

pub use across::SettledAcross;
pub use balance::{Balanced, LeadershipMoves, Moved, ShardSamples};
pub use coordinate::{Coordinate, Coordinated, Coordination, Surface};
pub use db::Db;
pub use tessari_lsm::{AtRestKey, Durability, StoreConfig};
pub use tessari_session::redact::{Visible, seen};
pub use tessari_session::travels;
pub use tessari_session::{
    AccessPath, AcrossRefusal, Detached, Error, Exactness, Nearest, Note, Outcome, Parameters,
    PartRefused, RefusalKind, Result, Session, Suggestion, Ticket, VaultAct, VaultTarget,
};
/// The store's own refusals, which [`Error::Store`] carries — named so a surface
/// can tell the one that means *go there* (`WriteIsElsewhere`) from the rest.
pub use tessari_storage::Error as StoreError;
pub use tessari_storage::{
    BUILD_VERSION, Change, ChangeKind, Changes, LeadershipDefinition, Lease, LogId, NODE_ID_LEN,
    Reach, Subscription, Upstream, Watch, Writer,
};
pub use tessari_types::{
    DatabaseId, Datetime, Duration, FieldKind, Geometry, NamespaceId, Number, Path as FieldPath,
    Polygon, Position, RecordId, RecordRef, Ring, Sequence, Step, TableId, Value, from_geojson,
    geojson_name, to_geojson,
};
pub use values::{NotAValue, value_of};
