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
use super::statement::NamedConsumer;

/// What `ACK` and `NACK` name: the topic, its partition, the group and the
/// positions.
type Settlement = (
    TableRef,
    Option<crate::ast::Expr>,
    NamedConsumer,
    Vec<crate::ast::Expr>,
);
use crate::ast::{
    GroupChange, GroupClauses, GroupMember, Hand, Handing, StatementKind, TableRef, TrimBound,
};
use crate::error::Result;
use crate::token::{Keyword, Punct, Token};

use tessari_types::Number;

/// The most messages a group may hold unacknowledged at once.
///
/// A group's in-flight set is kept in one record (ADR-0086), so its width is a
/// bound on that record's size as well as on the work outstanding.
pub const MAX_IN_FLIGHT: u64 = 10_000;

impl Parser<'_> {
    /// `DEFINE GROUP [IF NOT EXISTS] '<name>' ON TOPIC <topic> ACK DEADLINE d | NONE
    /// [DELIVERIES n] [IN FLIGHT n] [DEAD LETTER TO <topic>]`, the words
    /// `DEFINE GROUP` consumed.
    ///
    /// The optional clauses are order-free and each is accepted once, the rule
    /// `DEFINE TOPIC` follows. A dead letter with no delivery bound is refused
    /// where it was written: nothing would ever be sent to it.
    pub(super) fn define_group(&mut self) -> Result<StatementKind> {
        let if_not_exists = self.eat_if_not_exists()?;
        let ((name, name_parameter), topic) = self.group_on_topic()?;
        self.expect_word(
            "ack",
            "`ACK DEADLINE d` — how long a reader has before its message is handed out again",
        )?;
        self.expect_word("deadline", "`DEADLINE` and how long a reader has")?;
        // `NONE` holds a message until a statement settles it (ADR-0133).
        let deadline = if self.eat_keyword(Keyword::None) {
            None
        } else {
            Some(self.positive_duration(
                "a duration above zero, like `30s`, or `NONE` — how long a reader has to acknowledge",
            )?)
        };
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
            name_parameter,
            topic,
            if_not_exists,
            clauses,
        })
    }

    /// `DROP GROUP '<name>' ON TOPIC <topic>`, the words `DROP GROUP` consumed.
    pub(super) fn drop_group(&mut self) -> Result<StatementKind> {
        let ((name, name_parameter), topic) = self.group_on_topic()?;
        Ok(StatementKind::DropGroup {
            name,
            name_parameter,
            topic,
        })
    }

    /// `ALTER GROUP '<name>' ON TOPIC <topic> START AT <expr> | ADD MEMBER
    /// '<name>' | DROP MEMBER '<name>'`, the words `ALTER GROUP` consumed.
    pub(super) fn alter_group(&mut self) -> Result<StatementKind> {
        let ((name, name_parameter), topic) = self.group_on_topic()?;
        let change = if self.eat_word("add") {
            self.expect_word("member", "`MEMBER` and a quoted name after `ADD`")?;
            let (member, parameter) = self.consumer_name()?;
            GroupChange::AddMember(member, parameter)
        } else if self.eat_keyword(Keyword::Drop) {
            self.expect_word("member", "`MEMBER` and a quoted name after `DROP`")?;
            let (member, parameter) = self.consumer_name()?;
            GroupChange::DropMember(member, parameter)
        } else {
            self.expect_word(
                "start",
                "`START AT n`, `ADD MEMBER '<name>'` or `DROP MEMBER '<name>'`",
            )?;
            self.expect_word("at", "`AT` and a position")?;
            GroupChange::StartAt(self.expression()?)
        };
        Ok(StatementKind::AlterGroup {
            name,
            name_parameter,
            topic,
            change,
        })
    }

    /// `HAND <topic> [PARTITION <expr>] FOR CONSUMER '<group>' AT <expr>[, …] |
    /// AFTER <expr> LIMIT <expr> TO '<member>' [IF IDLE <expr>]`, `HAND`
    /// consumed (ADR-0133).
    pub(super) fn hand_statement(&mut self) -> Result<StatementKind> {
        let topic = self.table_ref()?;
        let partition = if self.eat_word("partition") {
            Some(self.expression()?)
        } else {
            None
        };
        let (Some(consumer), consumer_parameter) = self.for_consumer()? else {
            return Err(self.error_here("`FOR CONSUMER '<name>'` — the group holding the messages"));
        };
        let handing = if self.eat_word("after") {
            let after = self.expression()?;
            self.expect_word("limit", "`LIMIT n` — the most messages to hand")?;
            Handing::After {
                after: Box::new(after),
                limit: Box::new(self.expression()?),
            }
        } else {
            self.expect_word("at", "`AT` and the positions, or `AFTER p LIMIT n`")?;
            let mut positions = vec![self.expression()?];
            while self.eat_punct(Punct::Comma) {
                positions.push(self.expression()?);
            }
            Handing::At(positions)
        };
        self.expect_keyword(
            Keyword::To,
            "`TO '<member>'` — the member the messages go to",
        )?;
        let (member, member_parameter) = self.consumer_name()?;
        let idle = if self.eat_keyword(Keyword::If) {
            self.expect_word("idle", "`IDLE` and how long a message has waited")?;
            Some(self.expression()?)
        } else {
            None
        };
        Ok(StatementKind::HandTopic(Box::new(Hand {
            topic,
            partition,
            consumer,
            consumer_parameter,
            handing,
            member,
            member_parameter,
            idle,
        })))
    }

    /// `ACK <topic> [PARTITION <expr>] FOR CONSUMER '<name>' AT <expr>[, <expr>…]`, `ACK`
    /// consumed.
    pub(super) fn ack_statement(&mut self) -> Result<StatementKind> {
        let (topic, partition, (consumer, consumer_parameter), positions) = self.settlement()?;
        Ok(StatementKind::AckTopic {
            topic,
            consumer,
            consumer_parameter,
            positions,
            partition,
        })
    }

    /// `TRIM <topic> [PARTITION <expr>] KEEP <expr> | BEFORE <expr>`, `TRIM`
    /// consumed.
    pub(super) fn trim_statement(&mut self) -> Result<StatementKind> {
        let topic = self.table_ref()?;
        let partition = if self.eat_word("partition") {
            Some(self.expression()?)
        } else {
            None
        };
        let bound = if self.eat_word("keep") {
            TrimBound::Keep(self.expression()?)
        } else if self.eat_word("before") {
            TrimBound::Before(self.expression()?)
        } else {
            return Err(self.error_here(
                "`KEEP n` or `BEFORE p` — how many messages stay, or the first position that does",
            ));
        };
        Ok(StatementKind::TrimTopic {
            topic,
            partition,
            bound,
        })
    }

    /// `NACK <topic> [PARTITION <expr>] FOR CONSUMER '<name>' AT <expr>[, <expr>…] [DELAY d]`,
    /// `NACK` consumed.
    pub(super) fn nack_statement(&mut self) -> Result<StatementKind> {
        let (topic, partition, (consumer, consumer_parameter), positions) = self.settlement()?;
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
            consumer_parameter,
            positions,
            delay,
            partition,
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

    /// `MEMBER '<name>' [HELD]` after a reader's name, or nothing (ADR-0133).
    ///
    /// A member belongs to a group, so it needs the group's name before it, and
    /// what is held is a member's, so `HELD` needs the member.
    pub(super) fn group_member(&mut self, grouped: bool) -> Result<Option<Box<GroupMember>>> {
        if !self.eat_word("member") {
            if self.eat_word("held") {
                return Err(self
                    .error_here("`MEMBER '<name>'` before `HELD` — what is held is one member's"));
            }
            return Ok(None);
        }
        if !grouped {
            return Err(self.error_here(
                "`FOR CONSUMER '<group>'` before `MEMBER` — a member belongs to a group",
            ));
        }
        let (name, name_parameter) = self.consumer_name()?;
        Ok(Some(Box::new(GroupMember {
            name,
            name_parameter,
            held: self.eat_word("held"),
        })))
    }

    /// `INFO FOR GROUP '<name>' ON TOPIC <topic>`, the words `INFO FOR GROUP`
    /// consumed.
    pub(super) fn info_group(&mut self) -> Result<crate::ast::InfoSubject> {
        let ((name, name_parameter), topic) = self.group_on_topic()?;
        Ok(crate::ast::InfoSubject::Group {
            name,
            name_parameter,
            topic,
        })
    }

    /// `'<name>' ON TOPIC <topic>`.
    fn group_on_topic(&mut self) -> Result<(NamedConsumer, TableRef)> {
        let name = self.consumer_name()?;
        self.expect_keyword(Keyword::On, "`ON TOPIC` and the topic the group reads")?;
        self.expect_word("topic", "`TOPIC` and the topic the group reads")?;
        Ok((name, self.table_ref()?))
    }

    /// `<topic> [PARTITION <expr>] FOR CONSUMER '<name>' AT <expr>[, <expr>…]`.
    fn settlement(&mut self) -> Result<Settlement> {
        let topic = self.table_ref()?;
        let partition = if self.eat_word("partition") {
            Some(self.expression()?)
        } else {
            None
        };
        let (Some(consumer), parameter) = self.for_consumer()? else {
            return Err(
                self.error_here("`FOR CONSUMER '<name>'` — the group that handed the messages out")
            );
        };
        self.expect_word("at", "`AT` and the positions")?;
        let mut positions = vec![self.expression()?];
        while self.eat_punct(Punct::Comma) {
            positions.push(self.expression()?);
        }
        Ok((topic, partition, (consumer, parameter), positions))
    }
}
