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

mod accumulate;
mod aggregate;
mod arithmetic;
mod authorize;
mod budget;
mod call;
mod cast;
mod collection;
mod condition;
mod consume;
mod context;
mod describe;
mod digest;
mod effect;
mod elsewhere;
mod encoding;
mod error;
mod evaluate;
mod execute;
mod file;
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
mod search;
mod session;
mod shape;
mod text;
mod throttle;
mod ticket;
mod vector;
mod view;

pub use effect::{Effect, admits};
pub use elsewhere::{Elsewhere, Peer};
pub use error::{Depended, Error, Result};
pub use gather::{Asked, Gather, Gathered, Unanswered};
pub use outcome::{AccessPath, Exactness, Nearest, Note, Outcome, Suggestion};
pub use plan::Plan;
pub use session::Session;
pub use tessari_ql::Parameters;
pub use ticket::Ticket;
