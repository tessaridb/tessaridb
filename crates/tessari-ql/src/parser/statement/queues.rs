//! Queues and stream consumers: claims, releases and consumer declarations.

use super::Parser;
use tessari_types::Number;

use crate::ast::{ConsumerSource, FieldMapping, Name, OnFailure, StatementKind};
use crate::error::{Error, Result};
use crate::token::{Keyword, Punct, Span, Token};

impl Parser<'_> {
    /// The quoted name a session claims under.
    ///
    /// A literal rather than an identifier: it is the client's own string, not a
    /// catalog object, and nothing declares it first.
    pub(in crate::parser) fn consumer_name(&mut self) -> Result<String> {
        let Some(Token::Str(name)) = self.peek() else {
            return Err(self.error_here("a quoted consumer name"));
        };
        let name = name.clone();
        self.advance();
        if name.is_empty() {
            // An empty name would be a consumer that reads as *nobody said*,
            // which is what an ABSENT `claimed_by` already means. Two spellings
            // of one state is how a reader ends up asking which was meant.
            return Err(self.error_here("a consumer name with something in it"));
        }
        Ok(name)
    }

    /// `RELEASE jobs:7` · `RELEASE ALL FROM jobs [FOR CONSUMER 'billing']`
    pub(super) fn release_statement(&mut self, start: Span) -> Result<StatementKind> {
        if !self.eat_word("all") {
            let target = self.record_target()?;
            return Ok(StatementKind::Release {
                target,
                consumer: self.for_consumer()?,
                span: start.to(self.span_behind()),
            });
        }
        self.expect_keyword(Keyword::From, "`FROM` and the queue to release")?;
        let table = self.table_ref()?;
        let consumer = self.for_consumer()?;
        Ok(StatementKind::ReleaseAll {
            table,
            consumer,
            span: start.to(self.span_behind()),
        })
    }

    /// The optional `FOR CONSUMER '<name>'` both release forms accept.
    ///
    /// `for` is read as a contextual word, exactly as `INFO FOR` reads it, so
    /// neither `for` nor `consumer` is taken away from an application's own
    /// schema. One function rather than two copies, because the two statements
    /// mean the same thing by it: *the group's hold, not this instance's*.
    pub(in crate::parser) fn for_consumer(&mut self) -> Result<Option<String>> {
        if !self.eat_word("for") {
            return Ok(None);
        }
        self.expect_word("consumer", "`CONSUMER` and a quoted name after `FOR`")?;
        Ok(Some(self.consumer_name()?))
    }

    /// `CLAIM FROM jobs` · `CLAIM 10 FROM jobs`
    ///
    /// The count sits before `FROM` rather than in a `LIMIT` after the table,
    /// because it is not a ceiling on an answer that was going to be produced
    /// anyway — it is how much work this statement takes, and a `LIMIT` that
    /// decided how many records got written would be the one clause in the
    /// language that changes the store rather than the answer.
    pub(super) fn claim_statement(&mut self, start: Span) -> Result<StatementKind> {
        // Three shapes, told apart by the token after `CLAIM` rather than by a
        // keyword: a number commits to the counting form, `FROM` is that form
        // with a count of one, and anything else is a record the caller named.
        if matches!(self.peek(), Some(Token::Number(_))) {
            let count = self.claim_count()?;
            self.expect_keyword(Keyword::From, "`FROM` and the queue to take from")?;
            let table = self.table_ref()?;
            return Ok(StatementKind::Claim {
                table,
                count,
                span: start.to(self.span_behind()),
            });
        }
        if self.eat_keyword(Keyword::From) {
            let table = self.table_ref()?;
            return Ok(StatementKind::Claim {
                table,
                count: 1,
                span: start.to(self.span_behind()),
            });
        }
        let table = self.table_ref()?;
        // Raised here rather than by `record_target_after`, whose message is
        // shared with every other record-target statement. A bare `CLAIM jobs`
        // is most likely a forgotten `FROM` rather than a forgotten identity, so
        // the message names both forms instead of only what the parser wanted
        // next — and naming them here changes no other statement's refusal.
        if !self.at_punct(Punct::Colon) {
            return Err(self.error_here(
                "`:` and the record to hold, as in `CLAIM jobs:7` — or `FROM` \
                 and the queue to take from, as in `CLAIM FROM jobs`",
            ));
        }
        let target = self.record_target_after(table)?;
        Ok(StatementKind::ClaimRecord {
            target,
            span: start.to(self.span_behind()),
        })
    }

    /// The whole number after `CLAIM`.
    ///
    /// Zero is refused here because it is a **shape** mistake — a statement that
    /// asks for no records is not a claim — while the upper bound is refused by
    /// the store rather than by the grammar. That split is the one
    /// `DEFINE VECTOR` already makes about its distance: how much a store is
    /// willing to hand out in one statement is the store's question, and a
    /// ceiling compiled into the parser would be a second place holding it.
    pub(super) fn claim_count(&mut self) -> Result<u64> {
        let expected = "how many records to claim, like `10`";
        let Some(Token::Number(Number::Integer(written))) = self.peek() else {
            return Err(self.error_here(expected));
        };
        let Some(count) = u64::try_from(*written).ok().filter(|held| *held > 0) else {
            return Err(self.error_here(expected));
        };
        self.advance();
        Ok(count)
    }

    /// `DEFINE QUEUE jobs TIMEOUT 30s ATTEMPTS 5 SCHEMAFULL IN work`
    ///
    /// Two clauses and two flags, and they are read differently on purpose. The
    /// **clauses** — `TIMEOUT` and `ATTEMPTS` — keep their fixed order for the
    /// reason `DEFINE VECTOR`'s two do: they are what a queue is, two is too few
    /// to be worth an order-free reader, and a fixed order is what makes the
    /// statement read the same way in every store that has one. The **flags**
    /// are order-free against each other, because they are adjectives and
    /// `DEFINE TABLE` already reads its own that way.
    ///
    /// The timeout is a literal duration rather than an expression, the rule the
    /// query timeout already keeps: a budget a bound value could set is a budget
    /// a caller could raise, and this one is meant to be readable in the
    /// statement that declared it rather than in a bindings map somewhere else.
    pub(super) fn define_queue(&mut self) -> Result<StatementKind> {
        let if_not_exists = self.eat_if_not_exists()?;
        let name = self.name()?;
        if !self.eat_word("timeout") {
            return Err(self.error_here("`TIMEOUT` and how long a claim holds a record"));
        }
        let expected = "a duration, like `30s` or `5m`";
        let Some(Token::Duration(written)) = self.peek() else {
            return Err(self.error_here(expected));
        };
        let timeout = *written;
        let at = self.span_here();
        self.advance();
        // Refused here rather than at the claim, for the reason the query
        // timeout gives about its own zero: a hold of no length is not a hold,
        // so the declaration could only ever produce a queue that hands one
        // record to every worker at once — which is a mistake in the statement,
        // and the statement is where it is worth saying so.
        if timeout.seconds() < 0 || (timeout.seconds() == 0 && timeout.nanos() == 0) {
            return Err(Error::EmptyTimeout {
                written: timeout.to_literal(),
                span: at,
            });
        }
        let attempts = if self.eat_word("attempts") {
            Some(self.attempt_ceiling()?)
        } else {
            None
        };
        // The flags come after the clauses that are the subject of the
        // statement, and they are order-free against each other — both rules
        // read off `DEFINE TABLE`, where the same comment explains why: the
        // timeout and the ceiling are what a queue *is*, the flags are
        // adjectives on it, and a grammar that insisted on an order between two
        // adjectives would only be remembered wrong.
        let mut strictness: Option<bool> = None;
        let mut graph: Option<Name> = None;
        loop {
            if strictness.is_none() && self.eat_keyword(Keyword::Schemafull) {
                strictness = Some(true);
            } else if strictness.is_none() && self.eat_keyword(Keyword::Schemaless) {
                strictness = Some(false);
            } else if graph.is_none() && self.eat_keyword(Keyword::In) {
                graph = Some(self.name()?);
            } else {
                break;
            }
        }
        Ok(StatementKind::DefineQueue {
            name,
            timeout,
            attempts,
            // Lenient unless the word says otherwise, which is `DEFINE TABLE`'s
            // own default for a declaration carrying no columns. A queue never
            // carries any, so there is no reading under which it starts strict.
            schemafull: strictness.unwrap_or(false),
            graph,
            if_not_exists,
        })
    }

    /// The whole number after `ATTEMPTS`, which must be one and must be at least one.
    ///
    /// Zero is refused rather than read as unlimited. Unlimited already has a
    /// spelling — leaving the clause out — and a second one that looks like
    /// "never hand this out" would be the one somebody writes by accident.
    pub(super) fn attempt_ceiling(&mut self) -> Result<u32> {
        let expected = "how many times a record may be handed out, like `5`";
        let Some(Token::Number(Number::Integer(written))) = self.peek() else {
            return Err(self.error_here(expected));
        };
        // Read before advancing, so the refusal points at the number rather than
        // at whatever follows it.
        let Some(ceiling) = u32::try_from(*written).ok().filter(|held| *held > 0) else {
            return Err(self.error_here(expected));
        };
        self.advance();
        Ok(ceiling)
    }

    /// ```text
    /// DEFINE CONSUMER orders_in
    ///     FROM 'broker-1:9092', 'broker-2:9092'
    ///     TOPIC 'orders'
    ///     GROUP 'shop-orders'
    ///     FORMAT json
    ///     INTO shop.orders
    ///     IDENTITY order_id
    ///     MAP amount AS total, placed.at AS placed_at
    ///     ON FAILURE quarantine
    ///     PARALLELISM 2
    /// ```
    ///
    /// Every clause is required except `PARALLELISM`, and they are read in that
    /// order. A fixed order rather than a free one for `DEFINE NODE`'s reason in
    /// a stronger key: with nine clauses, accepting any order means the refusal
    /// for a missing one can no longer name it — the parser would only be able
    /// to say that *something* is missing, at the end, where the author has no
    /// idea which.
    ///
    /// **`on_failure` has no default.** A default here is a decision about data
    /// loss taken by whoever did not type the clause (ADR-0023 §4).
    pub(super) fn define_consumer(&mut self) -> Result<StatementKind> {
        let if_not_exists = self.eat_if_not_exists()?;
        let name = self.name()?;

        self.expect_keyword(Keyword::From, "`FROM` and the brokers to read from")?;
        let (first, _) = self.text("a broker, as text")?;
        let mut brokers = vec![first];
        while self.eat_punct(Punct::Comma) {
            let (broker, _) = self.text("a broker, as text")?;
            brokers.push(broker);
        }
        if !self.eat_word("topic") {
            return Err(self.error_here("`TOPIC` and the topic to read"));
        }
        let (topic, _) = self.text("the topic, as text")?;

        // Declared, never derived. Deriving it from the node id would be a bug
        // that only shows up in a cluster, where every node would form its own
        // group and every node would then consume every message.
        if !self.eat_word("group") {
            return Err(self.error_here("`GROUP` and the consumer group, as text"));
        }
        let (group, _) = self.text("the consumer group, as text")?;

        if !self.eat_word("format") {
            return Err(self.error_here("`FORMAT` and how a message becomes fields"));
        }
        let format = self.name()?;

        if !self.eat_word("into") {
            return Err(self.error_here("`INTO` and the table the records land in"));
        }
        let destination = self.table_ref()?;

        // Required, and it is what makes a replayed message converge to one
        // record instead of two — so it is the clause the at-least-once claim
        // rests on rather than an optional nicety.
        if !self.eat_word("identity") {
            return Err(
                self.error_here("`IDENTITY` and the message field carrying the record's id")
            );
        }
        let identity = self.field_path()?;

        if !self.eat_word("map") {
            return Err(
                self.error_here("`MAP` and which message fields become which record fields")
            );
        }
        let mut mapping = vec![self.field_mapping()?];
        while self.eat_punct(Punct::Comma) {
            mapping.push(self.field_mapping()?);
        }

        let (on_failure, parallelism) = self.failure_and_parallelism()?;

        Ok(StatementKind::DefineConsumer {
            name,
            source: ConsumerSource { brokers, topic },
            group,
            format,
            identity,
            mapping,
            destination,
            on_failure,
            parallelism,
            if_not_exists,
        })
    }

    /// `ON FAILURE stop|quarantine [PARALLELISM n]` — how a declared consumer of
    /// either kind treats a message it cannot apply, and how many members it runs.
    pub(super) fn failure_and_parallelism(&mut self) -> Result<(OnFailure, Option<u32>)> {
        self.expect_keyword(
            Keyword::On,
            "`ON FAILURE` and what to do with a message that cannot be applied",
        )?;
        if !self.eat_word("failure") {
            return Err(self.error_here("`FAILURE`, which follows `ON` here"));
        }
        let on_failure = if self.eat_word("stop") {
            OnFailure::Stop
        } else if self.eat_word("quarantine") {
            OnFailure::Quarantine
        } else {
            // Both named, and no third offered. A skip mode is the one this
            // grammar deliberately does not have.
            return Err(self.error_here("`STOP` or `QUARANTINE`"));
        };

        let parallelism = if self.eat_word("parallelism") {
            Some(self.whole_number("how many consumers to run")?)
        } else {
            None
        };
        Ok((on_failure, parallelism))
    }

    /// `amount AS total` — one message field and what the record calls it.
    pub(super) fn field_mapping(&mut self) -> Result<FieldMapping> {
        let from = self.field_path()?;
        self.expect_keyword(Keyword::As, "`AS` and what the record calls the field")?;
        Ok(FieldMapping {
            from,
            to: self.name()?,
        })
    }
}
