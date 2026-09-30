//! Running TessariQL scripts against a TessariDB store.
//!
//! This crate is where the language meets the store, and it is a thin layer on
//! purpose: it resolves names to ids, maps each statement onto a store call that
//! already exists, and owns the transaction boundary. It holds no planner, no
//! optimiser and no execution engine, because at this milestone the language is
//! bounded by what the store can already do (`docs/tessariql.md`).
//!
//! # The two properties worth stating
//!
//! **A statement outside `BEGIN` is its own transaction**, and inside one every
//! statement joins it. That is what lets a script define a table and write to it
//! and have both land or neither.
//!
//! **Names resolve per statement, inside the caller's transaction.** A session
//! remembers what `USE` selected by name and never caches the id, because a
//! cached id survives the table it named being dropped and re-created — and
//! reading the wrong table raises nothing.

#![forbid(unsafe_code)]
// `expect_used` and `as_conversions` govern production code; a test states its own expectations.
#![cfg_attr(test, allow(clippy::expect_used, clippy::as_conversions))]

mod accumulate;
mod aggregate;
mod arithmetic;
mod authorize;
mod backup_to;
mod budget;
mod call;
mod cast;
mod collection;
mod condition;
mod consume;
mod context;
mod describe;
mod detached;
mod digest;
mod effect;
mod elsewhere;
mod encoding;
mod error;
mod evaluate;
mod execute;
mod file;
mod fill;
mod gather;
mod generate;
mod geo;
mod geometry;
mod grants;
mod identity;
mod info;
mod kv;
mod noticed;
mod outcome;
mod plan;
mod queue;
mod rank;
mod reach;
pub mod redact;
mod reference;
mod restore;
mod rollup;
mod script;
mod search;
mod series;
mod session;
mod shape;
mod text;
mod throttle;
mod ticket;
mod topic;
mod vault_surface;
mod vector;
mod view;

pub use detached::Detached;
pub use effect::{Effect, admits};
pub use elsewhere::{Elsewhere, Peer};
pub use error::{Depended, Error, Result};
pub use gather::{Asked, Gather, Gathered, Unanswered};
pub use outcome::{AccessPath, Exactness, Nearest, Note, Outcome, Suggestion};
pub use plan::Plan;
pub use script::{ScriptTaken, write_script};
pub use session::{Atomic, Session};
pub use tessari_ql::Parameters;
pub use ticket::Ticket;
pub use vault_surface::{VaultAct, VaultTarget};
