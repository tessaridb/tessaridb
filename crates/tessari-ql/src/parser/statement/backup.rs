//! `BACKUP`: which backup, from where, and whether it is written into the
//! node's backup folder rather than answered with.

use super::Parser;

use crate::ast::StatementKind;
use crate::error::Result;
use crate::token::{Keyword, Token};

impl Parser<'_> {
    /// `BACKUP [STATE | SCRIPT | FROM n] [TO '<name>']`, from just after `BACKUP`.
    pub(in crate::parser) fn backup_statement(&mut self) -> Result<StatementKind> {
        self.advance();
        // `STATE` is contextual, as every word a statement's head adds
        // is: it is an ordinary field name everywhere else, and after
        // `BACKUP` nothing but this word or `FROM` can stand.
        let form = if self.eat_word("state") {
            Some(crate::ast::BackupForm::State)
        } else if self.eat_word("script") {
            Some(crate::ast::BackupForm::Script)
        } else {
            None
        };
        Ok(if let Some(form) = form {
            if self.eat_keyword(Keyword::From) {
                return Err(self.error_here(
                    "the end of the statement; a snapshot or a script is one moment, \
                     and `FROM` names a position in a log",
                ));
            }
            StatementKind::Backup {
                from: None,
                form,
                to: self.backup_to()?,
            }
        } else {
            // `FROM` reads as it does everywhere else — where the answer
            // starts — and leaving it out means the whole log, which is
            // what `write_from(.., 1)` already is.
            let from = if self.eat_keyword(Keyword::From) {
                let expected = "the sequence the backup starts at";
                let Some(Token::Number(tessari_types::Number::Integer(held))) = self.peek() else {
                    return Err(self.error_here(expected));
                };
                let held = u64::try_from(*held).map_err(|_| self.error_here(expected))?;
                self.advance();
                Some(held)
            } else {
                None
            };
            StatementKind::Backup {
                from,
                form: crate::ast::BackupForm::Log,
                to: self.backup_to()?,
            }
        })
    }

    /// The file a backup is written to, when the statement names one.
    ///
    /// A quoted string rather than an identifier: a path is the caller's text and
    /// no catalog object, and the node decides whether it stays inside the folder.
    fn backup_to(&mut self) -> Result<Option<String>> {
        if !self.eat_keyword(Keyword::To) {
            return Ok(None);
        }
        let Some(Token::Str(name)) = self.peek() else {
            return Err(self.error_here("the file name the backup is written to, quoted"));
        };
        let name = name.clone();
        self.advance();
        Ok(Some(name))
    }
}
