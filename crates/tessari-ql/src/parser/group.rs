//! Consumer groups on a topic (G042, ADR-0086): `DEFINE GROUP`, `DROP GROUP`,
//! `ALTER GROUP … START AT`, `ACK` and `NACK`.
//!
//! # Every word here is contextual
//!
//! `group`, `ack`, `nack`, `deadline`, `deliveries`, `flight`, `dead`,
//! `letter`, `delay` and `start` are ordinary field names, so none is reserved.
//! A group is named the way `FOR CONSUMER` names a reader — a quoted string —
//! because it is the same name: the readers of a group are the readers who read
//! under it.

use super::Parser;
use crate::ast::{GroupClauses, StatementKind, TableRef};
use crate::error::Result;
use crate::token::{Keyword, Punct, Token};

use tessari_types::Number;

/// The most messages a group may hold unacknowledged at once.
///
/// A group's in-flight set is kept in one record (ADR-0086), so its width is a
/// bound on that record's size as well as on the work outstanding.
pub const MAX_IN_FLIGHT: u64 = 10_000;

impl Parser<'_> {
    /// `DEFINE GROUP [IF NOT EXISTS] '<name>' ON TOPIC <topic> ACK DEADLINE d
    /// [DELIVERIES n] [IN FLIGHT n] [DEAD LETTER TO <topic>]`, the words
    /// `DEFINE GROUP` consumed.
    ///
    /// The optional clauses are order-free and each is accepted once, the rule
    /// `DEFINE TOPIC` follows. A dead letter with no delivery bound is refused
    /// where it was written: nothing would ever be sent to it.
    pub(super) fn define_group(&mut self) -> Result<StatementKind> {
        let if_not_exists = self.eat_if_not_exists()?;
        let (name, topic) = self.group_on_topic()?;
        self.expect_word(
            "ack",
            "`ACK DEADLINE d` — how long a reader has before its message is handed out again",
        )?;
        self.expect_word("deadline", "`DEADLINE` and how long a reader has")?;
        let deadline = self.positive_duration(
            "a duration above zero, like `30s` — how long a reader has to acknowledge",
        )?;
        let mut clauses = GroupClauses {
            deadline,
            deliveries: None,
            in_flight: None,
            dead_letter: None,
        };
        loop {
            if clauses.deliveries.is_none() && self.eat_word("deliveries") {
                clauses.deliveries = Some(self.positive_count(
                    "a whole number above zero — deliveries before a message is dead-lettered",
                )?);
            } else if clauses.in_flight.is_none() && self.eat_keyword(Keyword::In) {
                self.expect_word(
                    "flight",
                    "`FLIGHT` and how many messages may be held at once",
                )?;
                clauses.in_flight = Some(self.in_flight_width()?);
            } else if clauses.dead_letter.is_none() && self.eat_word("dead") {
                self.expect_word("letter", "`LETTER TO` and the topic dead letters go to")?;
                self.expect_keyword(Keyword::To, "`TO` and the topic dead letters go to")?;
                clauses.dead_letter = Some(self.table_ref()?);
            } else {
                break;
            }
        }
        if clauses.dead_letter.is_some() && clauses.deliveries.is_none() {
            return Err(self.error_here(
                "`DELIVERIES n` — a group with a dead letter must say after how many \
                 deliveries a message goes there",
            ));
        }
        Ok(StatementKind::DefineGroup {
            name,
            topic,
            if_not_exists,
            clauses,
        })
    }

    /// `DROP GROUP '<name>' ON TOPIC <topic>`, the words `DROP GROUP` consumed.
    pub(super) fn drop_group(&mut self) -> Result<StatementKind> {
        let (name, topic) = self.group_on_topic()?;
        Ok(StatementKind::DropGroup { name, topic })
    }

    /// `ALTER GROUP '<name>' ON TOPIC <topic> START AT <expr>`, the words
    /// `ALTER GROUP` consumed.
    pub(super) fn alter_group(&mut self) -> Result<StatementKind> {
        let (name, topic) = self.group_on_topic()?;
        self.expect_word(
            "start",
            "`START AT n` — the position the group has been given up to",
        )?;
        self.expect_word("at", "`AT` and a position")?;
        Ok(StatementKind::AlterGroup {
            name,
            topic,
            start_at: self.expression()?,
        })
    }

    /// `ACK <topic> FOR CONSUMER '<name>' AT <expr>[, <expr>…]`, `ACK`
    /// consumed.
    pub(super) fn ack_statement(&mut self) -> Result<StatementKind> {
        let (topic, consumer, positions) = self.settlement()?;
        Ok(StatementKind::AckTopic {
            topic,
            consumer,
            positions,
        })
    }

    /// `NACK <topic> FOR CONSUMER '<name>' AT <expr>[, <expr>…] [DELAY d]`,
    /// `NACK` consumed.
    pub(super) fn nack_statement(&mut self) -> Result<StatementKind> {
        let (topic, consumer, positions) = self.settlement()?;
        let delay = if self.eat_word("delay") {
            Some(self.positive_duration(
                "a duration above zero, like `5s` — how long before the message is handed out again",
            )?)
        } else {
            None
        };
        Ok(StatementKind::NackTopic {
            topic,
            consumer,
            positions,
            delay,
        })
    }

    /// The number after `IN FLIGHT`, refused where it stands when it is not
    /// from 1 to [`MAX_IN_FLIGHT`].
    fn in_flight_width(&mut self) -> Result<u64> {
        let expected = "a whole number from 1 to 10000 — messages held unacknowledged at once";
        let Some(Token::Number(Number::Integer(width))) = self.peek() else {
            return Err(self.error_here(expected));
        };
        let Some(width) = u64::try_from(*width)
            .ok()
            .filter(|width| (1..=MAX_IN_FLIGHT).contains(width))
        else {
            return Err(self.error_here(expected));
        };
        self.advance();
        Ok(width)
    }

    /// `'<name>' ON TOPIC <topic>`.
    fn group_on_topic(&mut self) -> Result<(String, TableRef)> {
        let name = self.consumer_name()?;
        self.expect_keyword(Keyword::On, "`ON TOPIC` and the topic the group reads")?;
        self.expect_word("topic", "`TOPIC` and the topic the group reads")?;
        Ok((name, self.table_ref()?))
    }

    /// `<topic> FOR CONSUMER '<name>' AT <expr>[, <expr>…]`.
    fn settlement(&mut self) -> Result<(TableRef, String, Vec<crate::ast::Expr>)> {
        let topic = self.table_ref()?;
        let Some(consumer) = self.for_consumer()? else {
            return Err(
                self.error_here("`FOR CONSUMER '<name>'` — the group that handed the messages out")
            );
        };
        self.expect_word("at", "`AT` and the positions")?;
        let mut positions = vec![self.expression()?];
        while self.eat_punct(Punct::Comma) {
            positions.push(self.expression()?);
        }
        Ok((topic, consumer, positions))
    }
}
