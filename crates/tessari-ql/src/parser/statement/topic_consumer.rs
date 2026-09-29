//! `DEFINE TOPIC CONSUMER` and the words that drop and describe one (ADR-0087).
//!
//! # Told apart from a topic called `consumer`
//!
//! After `TOPIC`, the word `consumer` starts a consumer only when what follows
//! could not follow a topic's name: `IF` (a topic's name comes after its own
//! `IF NOT EXISTS`, never before one) or, when declaring, a name and then `FROM`
//! — a topic's clauses are `RETAIN`, `MAX` and `PUBLIC`. When dropping or
//! describing, a second name after `consumer` decides it, since a topic is one
//! name and nothing follows it. So `DEFINE TOPIC consumer RETAIN 7d` and
//! `DROP TOPIC consumer` still mean the topic.

use super::Parser;
use crate::ast::StatementKind;
use crate::error::Result;
use crate::token::{Keyword, Punct, Token};

impl Parser<'_> {
    /// Whether `consumer` under the cursor starts a topic consumer, `TOPIC`
    /// already consumed. `declaring` is `DEFINE`; otherwise `DROP` or `INFO`.
    pub(super) fn topic_consumer_follows(&self, declaring: bool) -> bool {
        let is_word = |offset: usize, word: &str| matches!(self.peek_ahead(offset), Some(Token::Ident(found)) if found.eq_ignore_ascii_case(word));
        let is_name = |offset: usize| matches!(self.peek_ahead(offset), Some(Token::Ident(_)));
        let is_keyword =
            |offset: usize, keyword: Keyword| self.follows_with(offset, &Token::Keyword(keyword));
        if !is_word(0, "consumer") {
            return false;
        }
        if declaring {
            is_keyword(1, Keyword::If) || (is_name(1) && is_keyword(2, Keyword::From))
        } else {
            is_name(1)
        }
    }

    /// `DEFINE TOPIC CONSUMER [IF NOT EXISTS] <name> FROM <topic> GROUP '<g>'
    /// INTO <table> IDENTITY <path> MAP <from> AS <to>[, …] ON FAILURE
    /// stop|quarantine [PARALLELISM n]`, the words through `CONSUMER` consumed.
    ///
    /// The clauses are read in that order, as the Kafka form's are, so a
    /// missing one is named where it is missing.
    pub(super) fn define_topic_consumer(&mut self) -> Result<StatementKind> {
        let if_not_exists = self.eat_if_not_exists()?;
        let name = self.name()?;

        self.expect_keyword(Keyword::From, "`FROM` and the topic to read")?;
        let topic = self.table_ref()?;

        // Declared, never derived, and declared on the topic beforehand: the
        // group's deadline, width and dead letter are what this consumer reads
        // under, and none of them is a choice a consumer can guess.
        if !self.eat_word("group") {
            return Err(self.error_here("`GROUP` and the consumer group, as text"));
        }
        let (group, _) = self.text("the consumer group, as text")?;

        if self.peek_word("format") {
            return Err(self.error_here(
                "`INTO` — a topic's message is already a value, so a topic consumer has no `FORMAT`",
            ));
        }
        if !self.eat_word("into") {
            return Err(self.error_here("`INTO` and the table the records land in"));
        }
        let destination = self.table_ref()?;

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

        Ok(StatementKind::DefineTopicConsumer {
            name,
            topic,
            group,
            identity,
            mapping,
            destination,
            on_failure,
            parallelism,
            if_not_exists,
        })
    }
}
