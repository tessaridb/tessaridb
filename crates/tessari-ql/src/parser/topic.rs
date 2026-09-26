//! Topics (G037): `DEFINE TOPIC` and `READ FROM`.
//!
//! # Every word here is contextual
//!
//! `topic`, `retain`, `bytes`, `public`, `rate` and `per` are ordinary field
//! names, so none is reserved. `READ` already reads a file; `READ FROM` reads a
//! topic, and the two cannot be confused because a record reference never
//! begins with `FROM`.

use super::Parser;
use crate::ast::{StatementKind, TopicClauses};
use crate::error::Result;
use crate::token::{Keyword, Token};

use tessari_types::Duration;

impl Parser<'_> {
    /// `DEFINE TOPIC [IF NOT EXISTS] <name> [RETAIN d] [MAX BYTES n]
    /// [PUBLIC RATE n PER d]`, the words `DEFINE TOPIC` consumed.
    ///
    /// The clauses are order-free and each is accepted once, the rule
    /// `DEFINE TABLE`'s flags follow. A zero anywhere is refused here, where
    /// the statement that wrote it can be pointed at: a topic that keeps nothing
    /// for no time or takes messages of no size cannot hold anything.
    pub(super) fn define_topic(&mut self) -> Result<StatementKind> {
        let if_not_exists = self.eat_if_not_exists()?;
        let name = self.name()?;
        let mut clauses = TopicClauses::default();
        loop {
            if clauses.retain.is_none() && self.eat_word("retain") {
                clauses.retain = Some(self.positive_duration(
                    "a duration above zero, like `7d` — how long a message is kept",
                )?);
            } else if clauses.max_bytes.is_none() && self.peek_word("max") {
                self.eat_word("max");
                self.expect_word("bytes", "`BYTES` and the most bytes one message may take")?;
                clauses.max_bytes = Some(self.positive_count(
                    "a whole number above zero — the most bytes one message may take",
                )?);
            } else if clauses.public.is_none() && self.eat_word("public") {
                self.expect_word(
                    "rate",
                    "`RATE n PER d` — how often a caller nobody signed in may append",
                )?;
                let rate =
                    self.positive_count("a whole number above zero — how many appends per window")?;
                self.expect_word("per", "`PER` and the window the rate counts in")?;
                let per = self.positive_duration(
                    "a duration above zero, like `1m` — the window the rate counts in",
                )?;
                clauses.public = Some((rate, per));
            } else {
                break;
            }
        }
        // An open door with no bound on what comes through it is refused
        // where it was written, and the rate alone is not a bound on size.
        if clauses.public.is_some() && clauses.max_bytes.is_none() {
            return Err(self.error_here(
                "`MAX BYTES n` — a topic anonymous callers may append to must bound a message's size",
            ));
        }
        Ok(StatementKind::DefineTopic {
            name,
            if_not_exists,
            clauses,
        })
    }

    /// `READ FROM <topic> [FOR CONSUMER '<name>'] [AFTER <expr>] [LIMIT <expr>]`,
    /// `READ` consumed and `FROM` next.
    pub(super) fn read_topic(&mut self) -> Result<StatementKind> {
        self.expect_keyword(Keyword::From, "`FROM` and the topic")?;
        let topic = self.table_ref()?;
        let consumer = self.for_consumer()?;
        let after = if self.eat_word("after") {
            Some(self.expression()?)
        } else {
            None
        };
        let limit = if self.eat_word("limit") {
            Some(self.expression()?)
        } else {
            None
        };
        Ok(StatementKind::ReadTopic {
            topic,
            consumer,
            after,
            limit,
        })
    }

    fn positive_count(&mut self, expected: &'static str) -> Result<u64> {
        let Some(Token::Number(tessari_types::Number::Integer(count))) = self.peek() else {
            return Err(self.error_here(expected));
        };
        let Some(count) = u64::try_from(*count).ok().filter(|count| *count > 0) else {
            return Err(self.error_here(expected));
        };
        self.advance();
        Ok(count)
    }

    fn positive_duration(&mut self, expected: &'static str) -> Result<Duration> {
        let Some(Token::Duration(written)) = self.peek() else {
            return Err(self.error_here(expected));
        };
        let written = *written;
        if written.seconds() < 0 || (written.seconds() == 0 && written.nanos() == 0) {
            return Err(self.error_here(expected));
        }
        self.advance();
        Ok(written)
    }
}
