//! How long a read may run, how much it may hold, and what happens when it
//! passes either.
//!
//! # Refused, never truncated
//!
//! A read that reaches its ceiling fails. It does not answer with the records it
//! had, and there is no note saying it stopped early — because a partial answer
//! that looks whole is the failure this store spends its rules removing, and a
//! timeout is the cheapest place in a language to introduce one. The records are
//! already in hand and handing them back costs nothing; a caller counting them,
//! summing them or writing them somewhere would be wrong and would have no way
//! to find out.
//!
//! The refusal says how far it got, which is the same information a truncated
//! answer would have carried, in the one place a caller cannot mistake for the
//! result.
//!
//! # What it bounds, exactly
//!
//! The ceiling is checked **once per record, as the read produces it**. That is
//! where a long read spends its time — decoding a record, testing it, projecting
//! it, offering it to a sort — and it is a granularity the statement can be told
//! about honestly. It does not interrupt a single storage call, and it does not
//! reach a read standing in an expression, which has no channel to carry a
//! budget into (the same boundary a cross-type note runs into).
//!
//! So the promise is: a read that goes on producing records stops. A read
//! blocked in one call below the language does not, and no clause in this
//! grammar can make it — that is a property of the layer underneath.
//!
//! # Nesting narrows and never widens
//!
//! A subquery's own ceiling applies to the subquery, and the enclosing read's
//! ceiling still applies to it too: whichever expires first refuses. An inner
//! clause that could *raise* the budget its caller set would make the outer
//! statement's ceiling a suggestion, which is not what a ceiling is.
//!
//! # The second ceiling: how much a read may hold
//!
//! A read another statement holds is built whole in memory before that statement
//! asks anything of it, so an unbounded one is an unbounded allocation inside a
//! single statement. Where that read stands as a **source**, the grammar already
//! refuses it without a `LIMIT`. Where it stands in an **expression** it does
//! not, and that is the position [`Ceiling`] covers — the one the note channel
//! provably cannot reach, since the answer there is a value and a value has no
//! room beside it for anything to say.
//!
//! It is spent at the same place and in the same call as the deadline, because
//! the two ceilings answer the same shape of question about the same record.

use std::time::Instant;

use tessari_ql::{Select, Span, Timeout};

use crate::error::{Error, Result};

/// A moment a read must not still be producing records after.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Deadline {
    /// When the budget runs out.
    at: Instant,
    /// The ceiling as the statement wrote it, for the refusal to quote back.
    written: tessari_types::Duration,
    /// Where the clause is, for the refusal to point at.
    span: Span,
}

impl Deadline {
    /// The deadline a clause sets, under the one already in force.
    ///
    /// `None` in, `None` out, which is the ordinary read. Two ceilings give the
    /// **earlier** one — see the module note on why an inner clause may not
    /// widen an outer budget.
    pub(crate) fn under(within: Option<Self>, clause: Option<Timeout>) -> Option<Self> {
        let own = clause.and_then(Self::starting);
        match (within, own) {
            (Some(outer), Some(inner)) if inner.at < outer.at => Some(inner),
            (Some(outer), _) => Some(outer),
            (None, own) => own,
        }
    }

    /// The moment this clause's budget runs out, from now.
    ///
    /// `None` for a ceiling too far off to be an instant this machine can name.
    /// A budget that cannot be reached is the same thing as no budget, so it is
    /// reported as one rather than as an error about arithmetic.
    fn starting(clause: Timeout) -> Option<Self> {
        // The parser refuses a ceiling that is zero or negative, so the seconds
        // are known non-negative here and the conversion cannot be lossy.
        let seconds = u64::try_from(clause.after.seconds()).ok()?;
        let span = std::time::Duration::new(seconds, clause.after.nanos());
        Some(Self {
            at: Instant::now().checked_add(span)?,
            written: clause.after,
            span: clause.span,
        })
    }

    /// The refusal this deadline raises once it has passed.
    fn passed(self, produced: u64) -> Error {
        Error::TimedOut {
            after: self.written.to_literal(),
            produced,
            span: self.span,
        }
    }
}

/// The most records a materialised read may hold when nobody said how many.
///
/// High enough that no subquery written by hand meets it, low enough that this
/// node's memory stops being a function of a table's size. It is a value of this
/// layer and not of the grammar: nothing in the tree records it, so a statement
/// means the same thing on a node that raises it (Q-208).
const UNSTATED: u64 = 10_000;

/// How many records a read standing in an expression may hold, when it named no
/// bound itself.
///
/// # Why here and nowhere else
///
/// A source in parentheses is refused at parse time without a `LIMIT`, so by the
/// time this layer sees one the bound is already the author's own. An expression
/// is the position that rule does not reach, and it cannot be given the same
/// rule: a read there is usually a scalar — `LET $n = (SELECT count() FROM t)` —
/// and demanding `LIMIT 1` of every one of them would be noise around the common
/// case to bound the rare one.
///
/// # Why it refuses rather than truncating
///
/// Because the alternative is worse than the disease. Unbounded, that read costs
/// too much memory and answers **correctly**; a default that quietly kept the
/// first ten thousand would answer cheaply and *wrongly*, and a `count` over it
/// would be a confident number nobody could tell from the true one. Bounding
/// memory is worth doing; buying it with a wrong answer is not.
///
/// A note could not have rescued that even in principle here — the answer is a
/// value, and a value has no room beside it. So: the same stance the deadline
/// takes, and a refusal that names the one word lifting it.
///
/// # The two reads it does not touch
///
/// A read carrying its own `LIMIT` bounded itself, and this yields to it.
///
/// A read that **folds** — `count(*)`, a `GROUP BY` — is exempt for a sharper
/// reason: its answer does not grow with the table, so it is not the shape this
/// exists for, and refusing it would be refusing the commonest scalar read there
/// is. The word the refusal names would not even help, because `LIMIT` bounds
/// what a grouping *answers with* and not what it reads; a ceiling whose stated
/// escape does not lift it is a lie in the error message. What a fold holds
/// while folding is transient and is the same cost the identical statement pays
/// at the top level, where nothing refuses it either (Q-210).
///
/// # The exemption is now asked rather than assumed
///
/// Both halves of that paragraph were true of every fold the language had when
/// it was written, and both are **false of `collect`** — whose answer *is* the
/// collection, and what it holds while folding is the answer rather than
/// something transient. `SELECT collect(x) FROM huge` standing in an expression
/// is precisely the unbounded read this ceiling exists to refuse, and it was
/// being waved through by the word `fold`. `median` fails the second half only:
/// it must see every value to find the middle.
///
/// So the exemption asks [`Aggregate::retention`](tessari_ql::Aggregate::retention)
/// instead of asking whether a fold is present, and a read holding a whole-group
/// fold gets the ordinary ceiling. The refusal's stated escape is honest there:
/// `LIMIT` genuinely bounds what such a fold collects (Q-227).
#[derive(Debug, Clone, Copy)]
pub(crate) struct Ceiling {
    /// The most records the read may hold.
    most: u64,
    /// The whole-group fold this read holds, when it holds one.
    ///
    /// Carried so the refusal can name the fold and the escape that actually
    /// bounds it, rather than the one that bounds an ordinary read.
    collecting: Option<&'static str>,
    /// Where the read is, for the refusal to point at.
    span: Span,
}

impl Ceiling {
    /// The ceiling a held read runs under, or none when it needs no ceiling.
    pub(crate) fn over(read: &Select) -> Option<Self> {
        // Neither escape reaches a read that collects its group, so it is asked
        // first and gets the ceiling whatever else it says.
        if let Some(fold) = holds_its_group(read) {
            return Some(Self {
                most: UNSTATED,
                collecting: Some(fold),
                span: read.span,
            });
        }
        if read.limit.is_some() || crate::evaluate::groups(read) {
            return None;
        }
        Some(Self {
            most: UNSTATED,
            collecting: None,
            span: read.span,
        })
    }

    /// The refusal this ceiling raises once it is passed.
    ///
    /// Two refusals, because they name two different escapes and a refusal whose
    /// escape does not work is worse than none: it sends the author to write a
    /// clause that changes nothing and leaves them with no way to read the
    /// message as anything but wrong.
    const fn reached(self) -> Error {
        match self.collecting {
            Some(fold) => Error::UnboundedCollection {
                fold,
                most: self.most,
                span: self.span,
            },
            None => Error::Unbounded {
                most: self.most,
                span: self.span,
            },
        }
    }
}

/// The whole-group fold this read holds, if it holds one.
///
/// `Projection::All` cannot hold a fold at all, so only a projection of values
/// is asked. The fold's spelling comes back so the refusal can name it.
fn holds_its_group(read: &Select) -> Option<&'static str> {
    match &read.projection {
        tessari_ql::Projection::All => None,
        tessari_ql::Projection::Values { values, .. } => {
            crate::aggregate::retains_its_group(values)
        }
    }
}

/// One read's budget, and how much of the answer it has produced.
///
/// Borrowed by each consumer a read runs through, never copied into one: a read
/// may run two consumers in turn, and a duplicated counter would report half of
/// how far the read got.
#[derive(Debug)]
pub(crate) struct Budget {
    within: Option<Deadline>,
    holding: Option<Ceiling>,
    /// Records produced across the whole read, which is what the deadline's
    /// refusal reports.
    produced: u64,
    /// Records the stage now running has retained, which is what the held
    /// ceiling bounds.
    held: u64,
}

impl Budget {
    /// A budget under the ceilings in force, or an unbounded one when there are
    /// none.
    pub(crate) const fn of(within: Option<Deadline>, holding: Option<Ceiling>) -> Self {
        Self {
            within,
            holding,
            produced: 0,
            held: 0,
        }
    }

    /// Begin a stage of this read.
    ///
    /// The deadline is cumulative over the whole read; the held ceiling is not.
    /// A read may run two consumers in turn — a barrier collects, then an
    /// ordering stage re-offers what it collected — and those are the same
    /// records seen twice, not twice as many records held. Counting them twice
    /// would make a stated ceiling of ten thousand mean five thousand on one
    /// path and ten thousand on another, which is the kind of ceiling nobody can
    /// be told about honestly.
    pub(crate) const fn stage(&mut self) {
        self.held = 0;
    }

    /// Account for one record, and refuse the read if either ceiling is passed.
    ///
    /// A read with no ceiling pays one branch on `None`. A read with a deadline
    /// pays a clock read per record, which is what it asked for: a ceiling
    /// checked every thousandth record is a ceiling that can be overrun by a
    /// thousand records' work, and nothing in the statement says so.
    pub(crate) fn spend(&mut self) -> Result<()> {
        self.produced = self.produced.saturating_add(1);
        self.held = self.held.saturating_add(1);
        if let Some(ceiling) = self.holding {
            if self.held > ceiling.most {
                return Err(ceiling.reached());
            }
        }
        let Some(deadline) = self.within else {
            return Ok(());
        };
        if Instant::now() < deadline.at {
            return Ok(());
        }
        Err(deadline.passed(self.produced))
    }
}
