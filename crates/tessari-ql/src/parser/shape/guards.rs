//! The clauses that bound or guard a read: USING, TIMEOUT, STALENESS, ANSWERED BY, VERSION, bounds and depth.

use super::super::Parser;
use crate::ast::{Admitted, AnsweredBy, DeleteBound, Hop, Staleness, Timeout, Using, Version};
use crate::error::{Error, Result};
use crate::token::{Keyword, Token};
use tessari_types::Number;

impl Parser<'_> {
    /// `LIMIT 10` or `START 20`, when it is there.
    /// `<word> n` or `<word> $n` — a count written out, or one a parameter
    /// will supply when the script is bound (ADR-0124 D5).
    pub(crate) fn bound_count(
        &mut self,
        word: &str,
    ) -> Result<(Option<u64>, Option<crate::ast::CountParameter>)> {
        if let Some(Token::Parameter(name)) = self.peek_ahead(1)
            && self.peek_word(word)
        {
            let name = name.clone();
            self.advance();
            let span = self.span_here();
            self.advance();
            return Ok((None, Some(crate::ast::CountParameter { name, span })));
        }
        Ok((self.bound(word)?, None))
    }

    pub(crate) fn bound(&mut self, word: &str) -> Result<Option<u64>> {
        if !self.eat_word(word) {
            return Ok(None);
        }
        let expected = "a whole number";
        let Some(Token::Number(Number::Integer(count))) = self.peek() else {
            return Err(self.error_here(expected));
        };
        let count = u64::try_from(*count).map_err(|_| self.error_here(expected))?;
        self.advance();
        Ok(Some(count))
    }

    /// `USING <path>` or `USING INDEX <name>`, when it is there.
    ///
    /// The path word is taken as written and is **not** checked here. The set of
    /// words belongs to the store that reports them, and a copy of it in the
    /// grammar would be a second vocabulary of exactly the kind one plan
    /// structure exists to remove — so an unrecognised word is refused where the
    /// words live, before the read runs, naming the ones that exist.
    pub(crate) fn using(&mut self) -> Result<Option<Using>> {
        if !self.eat_word("using") {
            return Ok(None);
        }
        // `index` is both a path word and the keyword that introduces a named
        // index, so the two forms are told apart by what follows: a name means
        // `USING INDEX by_email`, and anything else means the path word. One
        // token of lookahead, and no spelling has to be given up — `USING index`
        // asks whether *an* index answered, `USING INDEX by_email` asks which.
        if self.peek_keyword() == Some(Keyword::Index)
            && matches!(self.peek_ahead(1), Some(Token::Ident(_)))
            && !self.opens_the_next_clause()
        {
            self.advance();
            return Ok(Some(Using::Index(self.name()?)));
        }
        // `word_or_name`, because several path words are also keywords —
        // `index`, `join`, `record` — and a clause that accepted only the ones
        // that happen not to be would be a vocabulary decided by the lexer.
        Ok(Some(Using::Path(self.word_or_name()?)))
    }

    /// `TIMEOUT 200ms`, when it is there.
    ///
    /// The duration is a literal rather than an expression, and a parameter is
    /// not accepted in its place. A ceiling that a bound value could set is a
    /// ceiling a caller could raise, and the statement is where this one is meant
    /// to be readable — an operator reading a slow query wants the budget in
    /// front of them, not in a bindings map somewhere else.
    pub(crate) fn timeout(&mut self) -> Result<Option<Timeout>> {
        if !self.eat_word("timeout") {
            return Ok(None);
        }
        let expected = "a duration, like `200ms` or `5s`";
        let Some(Token::Duration(after)) = self.peek() else {
            return Err(self.error_here(expected));
        };
        let after = *after;
        let at = self.span_here();
        self.advance();
        // A ceiling of zero or less is refused here rather than at the read.
        // Neither names a budget a statement could satisfy, so the clause could
        // only ever refuse — and a clause that can only refuse is a mistake in
        // the statement, which is a thing to say when the statement is read.
        if after.seconds() < 0 || (after.seconds() == 0 && after.nanos() == 0) {
            return Err(Error::EmptyTimeout {
                written: after.to_literal(),
                span: at,
            });
        }
        Ok(Some(Timeout { after, span: at }))
    }

    /// `STALENESS 30s`, when it is there.
    ///
    /// Contextual the way `timeout` is, and with the same guard: a field called
    /// `staleness` is at least as likely as one called `timeout`, so the word
    /// opens a clause only when a duration follows it.
    ///
    /// A literal rather than an expression, and no parameter in its place. A
    /// tolerance a bound value could set is a tolerance a caller could widen,
    /// and the point of the clause is that the statement says out loud how stale
    /// an answer it will take.
    pub(crate) fn staleness(&mut self) -> Result<Option<Staleness>> {
        if !self.eat_word("staleness") {
            return Ok(None);
        }
        let expected = "a duration, like `30s` or `5m`";
        let Some(Token::Duration(within)) = self.peek() else {
            return Err(self.error_here(expected));
        };
        let within = *within;
        let at = self.span_here();
        self.advance();
        // Refused here rather than at the read, for `timeout`'s reason: a
        // tolerance of zero or less admits no node at all — not even the one
        // being asked — so the clause could only ever refuse, and a clause that
        // can only refuse is a mistake in the statement.
        if within.seconds() < 0 || (within.seconds() == 0 && within.nanos() == 0) {
            return Err(Error::EmptyStaleness {
                written: within.to_literal(),
                span: at,
            });
        }
        Ok(Some(Staleness { within, span: at }))
    }

    /// `ANSWERED BY LEADER`, when it is there.
    ///
    /// Two words at the head, the way `GROUP BY` and `ORDER BY` are, and both
    /// are required before the clause opens: a lone `answered` is not this
    /// clause, and saying so with a peek rather than with a rollback is what
    /// keeps a name called `answered` readable everywhere else.
    ///
    /// # Why an unknown word is refused and never defaulted
    ///
    /// The two admitted spellings are a closed set, and the direction a guess
    /// would fail in is the unsafe one: a caller who wrote `ANSWERED BY MASTER`
    /// asked for the leader, and a parser that shrugged and admitted any copy
    /// would answer the financial read from a follower with nothing anywhere in
    /// an error state. So the refusal carries the word that was written and the
    /// span it sits at, which is what lets a caller fix it without guessing.
    pub(crate) fn answered_by(&mut self) -> Result<Option<AnsweredBy>> {
        if !matches!(self.peek(), Some(Token::Ident(word)) if word.eq_ignore_ascii_case("answered"))
            || !matches!(self.peek_ahead(1), Some(Token::Ident(word)) if word.eq_ignore_ascii_case("by"))
        {
            return Ok(None);
        }
        let at = self.span_here();
        self.advance();
        self.advance();
        let expected = "`ANY` or `LEADER` — which nodes may answer this read";
        let Some(Token::Ident(written)) = self.peek() else {
            return Err(self.error_here(expected));
        };
        let admits = if written.eq_ignore_ascii_case("any") {
            Admitted::AnyCopy
        } else if written.eq_ignore_ascii_case("leader") {
            Admitted::Leader
        } else {
            return Err(Error::UnknownAnswerer {
                written: written.to_string(),
                span: self.span_here(),
            });
        };
        let at = at.to(self.span_here());
        self.advance();
        Ok(Some(AnsweredBy { admits, span: at }))
    }

    /// `VERSION 42`, when it is there.
    ///
    /// Contextual for the same reason `timeout` is, and with the same guard: a
    /// field called `version` is at least as likely as one called `timeout`, so
    /// the word opens a clause only when an integer follows it and reads as a
    /// name everywhere else.
    ///
    /// A literal rather than an expression, and no parameter in its place. The
    /// point of the read is that it is reproducible — the same statement asked
    /// twice answers the same — and a version a binding could set is one a caller
    /// could move between two runs of the statement that names it.
    pub(crate) fn version(&mut self) -> Result<Option<Version>> {
        if !matches!(self.peek(), Some(Token::Ident(word)) if word.eq_ignore_ascii_case("version"))
            || !matches!(self.peek_ahead(1), Some(Token::Number(Number::Integer(_))))
        {
            return Ok(None);
        }
        let at = self.span_here();
        self.advance();
        let expected = "a sequence, like `42` — the version a read answers from";
        let Some(Token::Number(Number::Integer(sequence))) = self.peek() else {
            return Err(self.error_here(expected));
        };
        let sequence = *sequence;
        self.advance();
        // A negative sequence names no point in any store's history. Refused
        // here rather than at the read, for the reason an empty timeout is: a
        // clause that could only ever be refused is a mistake in the statement,
        // and the statement is where it should be said.
        let at_sequence = u64::try_from(sequence).map_err(|_| self.error_here(expected))?;
        Ok(Some(Version {
            at: at_sequence,
            span: at,
        }))
    }

    /// The bound a delete over a **set** must carry: `LIMIT 100` or `LIMIT ALL`.
    ///
    /// Required, unlike every other `LIMIT` in this grammar. A read that omits
    /// one answers with more rows than the caller expected; a delete that omits
    /// one removes a table. `LIMIT ALL` is the way to say the second on purpose,
    /// and it costs one word — which is the entire mechanism.
    pub(crate) fn delete_bound(&mut self) -> Result<DeleteBound> {
        let expected =
            "`LIMIT n` or `LIMIT ALL` — a delete over a set states how much it may remove";
        if !self.eat_word("limit") {
            return Err(self.error_here(expected));
        }
        if self.eat_word("all") {
            return Ok(DeleteBound::All);
        }
        let Some(Token::Number(Number::Integer(count))) = self.peek() else {
            return Err(self.error_here(expected));
        };
        let count = u64::try_from(*count).map_err(|_| self.error_here(expected))?;
        self.advance();
        Ok(DeleteBound::AtMost(count))
    }

    /// `DEPTH 3` at the end of a walk, when it is there.
    ///
    /// The number is an integer **literal** and the grammar has no position here
    /// for anything else — not a parameter, not an expression, not a field. That
    /// is the whole of what the clause guarantees: every walk this language can
    /// write states its own length, and a reader of the statement knows how far
    /// it goes without knowing what the caller bound.
    ///
    /// A parameter would look harmless and would not be. `DEPTH $n` is a walk
    /// whose length arrives at run time from somewhere the statement cannot
    /// show, which is the same shape as an unbounded walk with a promise
    /// attached — and the promise is kept by whoever wrote the caller.
    pub(crate) fn depth_bound(&mut self, hops: &[Hop]) -> Result<Option<u64>> {
        if !self.eat_word("depth") {
            return Ok(None);
        }
        let span = self.span_here();
        // Checked before the number is read, so `DEPTH x` on a chain refuses as
        // the chain it is rather than as a missing integer — the first fault a
        // reader can act on is the one worth reporting.
        if hops.len() != 1 || hops.first().is_none_or(|hop| hop.target.is_none()) {
            return Err(Error::DepthNeedsOneHopToATable { span });
        }
        let Some(Token::Number(Number::Integer(count))) = self.peek() else {
            return Err(self.error_here("`DEPTH n` — a whole number of steps, written out"));
        };
        // A negative and a zero refuse as the same thing, which they are: the
        // clause counts steps, and both say fewer than one.
        let count = u64::try_from(*count).unwrap_or(0);
        self.advance();
        if count == 0 {
            return Err(Error::DepthBelowOne { span });
        }
        Ok(Some(count))
    }
}
