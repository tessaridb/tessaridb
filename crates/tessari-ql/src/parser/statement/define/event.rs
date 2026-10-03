//! `DEFINE EVENT` — what runs after a record write (ADR-0110).

use std::collections::BTreeMap;

use tessari_types::{Value, WriteKind};

use super::super::Parser;
use crate::ast::{Script, Statement, StatementKind};
use crate::error::{Error, Result};
use crate::token::{Keyword, Punct, Token};

/// The names an event binds for its condition and its body.
pub const EVENT_BINDINGS: [&str; 4] = ["event", "before", "after", "id"];

impl Parser<'_> {
    /// `DEFINE EVENT [IF NOT EXISTS] audit ON orders [FOR CREATE, UPDATE]
    /// [WHEN $after.total > 100] THEN CREATE log = { … }` — or `THEN { … }`
    /// for several statements.
    ///
    /// `event`, `for` and `when` are contextual words: each is an ordinary
    /// field name in stores that already use it.
    ///
    /// # Checked here, kept as text
    ///
    /// The condition and every statement of the body are parsed, each body
    /// statement's kind is checked against what an event may run, and both are
    /// bound against the four names the event supplies — so a body naming
    /// `$aftr` is refused where it is written rather than on the first write it
    /// would have refused. What the statement then carries is the source text,
    /// as a view's read is.
    pub(crate) fn define_event(&mut self) -> Result<StatementKind> {
        let if_not_exists = self.eat_if_not_exists()?;
        let name = self.name()?;
        self.expect_keyword(Keyword::On, "`ON` and the table whose writes run it")?;
        let table = self.table_ref()?;
        let mut on = Vec::new();
        if self.eat_word("for") {
            loop {
                let kind = match self.peek_keyword() {
                    Some(Keyword::Create) => WriteKind::Create,
                    Some(Keyword::Update) => WriteKind::Update,
                    Some(Keyword::Delete) => WriteKind::Delete,
                    _ => return Err(self.error_here("`CREATE`, `UPDATE` or `DELETE`")),
                };
                self.advance();
                if !on.contains(&kind) {
                    on.push(kind);
                }
                if !self.eat_punct(Punct::Comma) {
                    break;
                }
            }
            on.sort_unstable();
        } else {
            on.extend(WriteKind::ALL);
        }
        let when = if self.eat_word("when") {
            Some(self.written_expression()?)
        } else {
            None
        };
        self.expect_keyword(Keyword::Then, "`THEN` and what the event does")?;
        let (statements, body) = if self.eat_punct(Punct::BraceOpen) {
            let start = self.span_here().start;
            let mut statements = Vec::new();
            while self.peek() != Some(&Token::Punct(Punct::BraceClose)) {
                statements.push(self.event_statement()?);
                if !self.eat_punct(Punct::Semicolon) {
                    break;
                }
            }
            let end = self.span_here().start;
            self.expect_punct(Punct::BraceClose, "`}` after the event's statements")?;
            let text = self.source.get(start..end).unwrap_or_default().trim();
            (statements, text.trim_end_matches(';').trim_end().to_owned())
        } else {
            let statement = self.event_statement()?;
            let text = self
                .source
                .get(statement.span.start..statement.span.end)
                .unwrap_or_default()
                .to_owned();
            (vec![statement], text)
        };
        if statements.is_empty() {
            return Err(self.error_here("at least one statement for the event to run"));
        }
        // Bound once against stand-ins for the four names, to refuse a
        // parameter nothing supplies; the values themselves arrive per write.
        let mut checked = statements;
        if let Some(condition) = &when {
            checked.push(Statement {
                kind: StatementKind::Return {
                    value: crate::parser::parse_expression(&condition.text)?,
                },
                span: condition.span,
                acknowledge: None,
                across: false,
            });
        }
        // `$id` stands in as an identity, because it is written where one is
        // read (`orders:$id`) and `NONE` would be refused there.
        let stand_ins: BTreeMap<String, Value> = EVENT_BINDINGS
            .iter()
            .map(|name| {
                let stand_in = if *name == "id" {
                    Value::from(0_i64)
                } else {
                    Value::None
                };
                ((*name).to_owned(), stand_in)
            })
            .collect();
        Script {
            span: self.span_behind(),
            statements: checked,
        }
        .bind(&stand_ins)?;
        Ok(StatementKind::DefineEvent {
            name,
            table,
            on,
            when: when.map(|condition| condition.text),
            body,
            if_not_exists,
        })
    }

    /// One statement of an event's body, refused where an event cannot run it
    /// (ADR-0110 D7).
    fn event_statement(&mut self) -> Result<Statement> {
        let at = self.span_here();
        let statement = self.statement()?;
        let runs = matches!(
            statement.kind,
            StatementKind::Create { .. }
                | StatementKind::Insert { .. }
                | StatementKind::Update { .. }
                | StatementKind::Upsert { .. }
                | StatementKind::Delete { .. }
                | StatementKind::DeleteEdge { .. }
                | StatementKind::DeleteWhere { .. }
                | StatementKind::DeleteSpan { .. }
                | StatementKind::Relate { .. }
                | StatementKind::Set { .. }
                | StatementKind::Incr { .. }
                | StatementKind::Expire { .. }
                | StatementKind::Persist { .. }
                | StatementKind::Del { .. }
                | StatementKind::Throw { .. }
                | StatementKind::Let { .. }
        );
        if runs {
            return Ok(statement);
        }
        let word = self
            .source
            .get(at.start..at.end)
            .unwrap_or_default()
            .to_uppercase();
        Err(Error::EventBody {
            statement: word,
            span: at,
        })
    }
}
