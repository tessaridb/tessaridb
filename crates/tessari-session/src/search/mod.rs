//! Asking whether text holds a word, how much that is worth, and where each
//! answer comes from.
//!
//! The analyzer is read from the **schema**, not from an index, which is the
//! whole of why full-text search here cannot behave differently once an index
//! exists. See [`tessari_types::Analyzer`] for the argument; this module is where
//! that decision is spent.
//!
//! # Two questions, resolved together and answered differently
//!
//! `MATCHES` asks about one document and needs only the analyzer. `search::score`
//! asks about a document *relative to a collection* and additionally needs what
//! the collection looks like — how many documents there are, how long a typical
//! one is, and how many hold each of the query's words. See [`crate::rank`] for
//! why that difference decides whether an index is optional or required.
//!
//! Both are resolved **once per read** rather than once per record, because the
//! schema does not change under a read and neither does the collection. They
//! travel together in [`Searched`] because they are needed in the same places
//! and have the same lifetime: a sort key is an expression too, and a `MATCHES`
//! or a score in one must mean what it means in the `WHERE` that produced the
//! records.
//!
//! # Three questions, three files
//!
//! The module is split by *which question a function answers*, not by size:
//! [`resolve`] is what one read needs before any record exists, [`query`] is
//! what the query string asked for, and [`matching`] is whether one document
//! holds it. The boundary is worth keeping because the first depends on the
//! catalog, the second must never depend on it, and the third is the scan half
//! of a pair whose index half has to agree with it record for record.

mod matching;
mod query;
mod resolve;
mod suggest;

pub(crate) use matching::{matches_fuzzy_terms, matches_prefix_terms, matches_terms};
pub(crate) use query::{Asked, asked};
pub(crate) use resolve::{Ranked, Searched};
