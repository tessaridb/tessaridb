//! `DEFINE FEED` — the declared capture of a table's writes into a topic.

use tessari_types::WriteKind;

use super::super::Parser;
use crate::ast::{FeedImage, StatementKind};
use crate::error::Result;
use crate::token::Keyword;

impl Parser<'_> {
    /// `DEFINE FEED [IF NOT EXISTS] <name> ON <table> INTO <topic>
    /// [FOR CREATE, UPDATE, DELETE] [IMAGE KEYS | FULL] [WHEN <cond>]
    /// [WITH INITIAL SCAN]`.
    ///
    /// `for`, `when` and `with` are contextual words: each is an ordinary
    /// field name in stores that already use it. `feed` itself is contextual
    /// the same way, so a table named `feed` keeps its name.
    ///
    /// # Checked here, kept as text
    ///
    /// The `WHEN` condition is parsed and bound against the four names a
    /// captured write supplies (`$event`, `$before`, `$after`, `$id`), as an
    /// event's is — so a condition naming `$aftr` is refused where it is
    /// written. What the statement then carries is the source text.
    pub(crate) fn define_feed(&mut self) -> Result<StatementKind> {
        let if_not_exists = self.eat_if_not_exists()?;
        let name = self.name()?;
        self.expect_keyword(Keyword::On, "`ON` and the table whose writes it captures")?;
        let table = self.table_ref()?;
        if !self.eat_word("into") {
            return Err(self.error_here("`INTO` and the topic the messages land in"));
        }
        let topic = self.table_ref()?;
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
                if !self.eat_punct(crate::token::Punct::Comma) {
                    break;
                }
            }
            on.sort_unstable();
        } else {
            on.extend(WriteKind::ALL);
        }
        let image = if self.eat_word("image") {
            // `KEYS` is a keyword of the language (a space walk names it), so
            // it does not match the contextual-word reader; `FULL` is not.
            if self.eat_keyword(Keyword::Keys) {
                FeedImage::Keys
            } else if self.eat_word("full") {
                FeedImage::Full
            } else {
                return Err(self.error_here("`KEYS` or `FULL` after `IMAGE`"));
            }
        } else {
            FeedImage::Full
        };
        let when = if self.eat_word("when") {
            let written = self.written_expression()?;
            // Bound against stand-ins for the names a captured write supplies,
            // as an event's condition is, so a name the write does not supply
            // is refused where it is written rather than on the first write.
            let expr = crate::parser::parse_expression(&written.text)?;
            let mut names = std::collections::BTreeMap::new();
            for (binding, stand_in) in [
                ("event", tessari_types::Value::from("CREATE")),
                ("before", tessari_types::Value::None),
                ("after", tessari_types::Value::None),
                ("id", tessari_types::Value::from(0_i64)),
            ] {
                names.insert(binding.to_owned(), stand_in);
            }
            crate::bind::bind_expression(expr, &names)?;
            Some(written.text)
        } else {
            None
        };
        let initial_scan = if self.eat_word("with") {
            if !self.eat_word("initial") {
                return Err(self.error_here("`INITIAL` after `WITH`"));
            }
            if !self.eat_word("scan") {
                return Err(self.error_here("`SCAN` after `WITH INITIAL`"));
            }
            true
        } else {
            false
        };
        Ok(StatementKind::DefineFeed {
            name,
            table,
            topic,
            on,
            image,
            when,
            initial_scan,
            if_not_exists,
        })
    }
}
