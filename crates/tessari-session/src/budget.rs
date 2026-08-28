//! How long a read may run, and what happens when it does not finish.
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

use std::time::Instant;

use tessari_ql::{Span, Timeout};

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

/// One read's budget, and how much of the answer it has produced.
///
/// Borrowed by each consumer a read runs through, never copied into one: a read
/// may run two consumers in turn, and a duplicated counter would report half of
/// how far the read got.
#[derive(Debug)]
pub(crate) struct Budget {
    within: Option<Deadline>,
    produced: u64,
}

impl Budget {
    /// A budget under the deadline in force, or an unbounded one when there is
    /// none.
    pub(crate) const fn of(within: Option<Deadline>) -> Self {
        Self {
            within,
            produced: 0,
        }
    }

    /// Account for one record, and refuse the read if the ceiling has passed.
    ///
    /// A read with no ceiling pays one branch on `None`. A read with one pays a
    /// clock read per record, which is what it asked for: a ceiling checked
    /// every thousandth record is a ceiling that can be overrun by a thousand
    /// records' work, and nothing in the statement says so.
    pub(crate) fn spend(&mut self) -> Result<()> {
        self.produced = self.produced.saturating_add(1);
        let Some(deadline) = self.within else {
            return Ok(());
        };
        if Instant::now() < deadline.at {
            return Ok(());
        }
        Err(deadline.passed(self.produced))
    }
}
