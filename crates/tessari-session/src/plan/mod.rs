//! Choosing which index runs, rather than taking the first one written.
//!
//! # What this changes, and what it deliberately cannot
//!
//! A condition may offer several conjuncts an index could serve. Until this
//! module existed the first one in the statement won, which meant
//! `WHERE city = 'x' AND email = 'ada@example.com'` read the `city` index even
//! though `email` is unique and selects exactly one record.
//!
//! That was never a *wrong answer* — candidates are re-tested against the whole
//! condition, so the rows come back right whichever index narrowed them. It was
//! a wrong **cost**, and a wrong cost raises nothing, which is why it survived
//! several waves. The rule that replaces it is stated here in one place so that
//! it can be read, argued with, and tested without a store.
//!
//! What it cannot change is the answer. The chosen candidate narrows; the
//! condition still decides. That is the store's governing rule, and a planner is
//! precisely the component most tempted to break it.
//!
//! # The parts, and the question each one answers
//!
//! | module | question |
//! |---|---|
//! | [`candidate`] | what an index can be asked to serve, and what it promises |
//! | [`conjunct`] | which conjuncts of a condition are worth offering an index |
//! | [`reads`] | what an expression reads — the record at all, and which fields |
//! | [`serving`] | which declared indexes can answer about a given path |
//! | [`enumerate`] | every candidate a condition and a schema offer together |
//! | [`rank`] | which of them promises to narrow the most |
//! | [`fold`] | evaluating the record-independent parts of a statement once |
//! | [`statement`] | what shape of read a whole statement is — nearest, ordered, bounded |
//! | [`reported`] | the one structure both `EXPLAIN` and an answer report |
//! | [`explain`] | reporting the plan a read would take, without taking it |
//!
//! The dependency runs one way: `reads` → `conjunct` → `enumerate` → `rank`,
//! with `serving` and `candidate` beneath the middle of it and `explain` on top
//! calling the same `enumerate` and the same `choose` the read calls. Two
//! planners that agree today would disagree the first time one changed.
//!
//! # Exact numbers only where they are free
//!
//! A planner that counts every candidate pays for each answer twice: counting the
//! records under a secondary index's value costs the same scan as reading them.
//! So the ranking uses a real number only where knowing it is free, and a
//! declared ordering everywhere else.
//!
//! | Candidate | Rows | What knowing that costs |
//! |---|---|---|
//! | equality on a **unique** index | at most 1 | nothing — it is what unique means |
//! | `MATCHES` on a search index | at most the smallest term's `df` | one prefix count per term |
//! | equality on a secondary index | unknown | the read itself |
//! | `LIKE 'a%'` prefix range | unknown, possibly the whole table | the read itself |
//!
//! The search bound is only cheap because SGC.T3 made a document frequency a
//! count of keys rather than a set of decoded record ids. The two nodes compose
//! by accident of good luck rather than design, and it is worth saying so: had
//! `df` stayed expensive, a search candidate would rank by shape like the others.
//!
//! # Why rule-based and not cost-based
//!
//! A cost model needs statistics about *value distribution* — how many records
//! hold `city = 'london'` as against `city = 'tromsø'` — and that means
//! histograms. A histogram is maintained state whose staleness silently changes
//! plans, which is a much larger decision than this one and wants a benchmark
//! harness (SGG.T1) to justify it rather than an intuition.
//!
//! # Ties break on source order
//!
//! Not arbitrarily, and not on index id: two runs of one statement must choose
//! the same way, and an author who reads their own condition should be able to
//! predict which of two equal candidates wins.
//!
//! One rule sits above it, and only because it is a **proof** rather than a
//! preference: a candidate narrowing more of its index's columns cannot return
//! more records than one narrowing fewer of them, since its entries are a
//! subset. An equal count still falls through to the order the conjuncts were
//! written.
//!
//! That proof sits above the **shape** ranking too, and it has to. The shape
//! order in `Shape` describes what a candidate is *trusted* to narrow when
//! nothing exact is known, which is a heuristic and is stated as one. Below the
//! proof it made a composite range unreachable: `a = 1 AND b > 2` on `(a, b)`
//! narrows two columns as a range and one as an equality, and `Equality` sorts
//! before `Range`, so the wider candidate won on the guess. Nothing this store
//! chose before moves, because every candidate that narrowed more than one
//! column was an equality — which the shape order already preferred.
//!
//! This is the rule the gathering restructuring most easily loses. Asking each
//! index what the whole condition fixes for it invites an outer loop over
//! *indexes*, which would make an equal tie resolve by declaration order — a
//! schema fact the author of the condition cannot see. So the gathering is by
//! index and the emitting is by conjunct, which keeps both properties at once.

mod candidate;
mod conjunct;
mod enumerate;
mod explain;
mod fold;
mod rank;
mod reads;
pub(crate) mod reported;
mod serving;
mod statement;
#[cfg(test)]
mod tests;

pub(crate) use candidate::{Candidate, Served};
pub(crate) use rank::choose;
pub(crate) use reads::roots_read;
pub use reported::Plan;
pub(crate) use statement::{
    Bounded, Closest, Nearest, Scored, answers, bound, closest, nearest, ordered, scored,
};
