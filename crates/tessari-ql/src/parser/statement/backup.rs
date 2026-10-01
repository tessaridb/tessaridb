//! `BACKUP`: which backup, from where, and whether it is written into the
//! node's backup folder rather than answered with.

use super::Parser;

use crate::ast::{ReachRef, StatementKind};
use crate::error::Result;
use crate::token::{Keyword, Punct, Token};

impl Parser<'_> {
    /// `BACKUP [STATE | SCRIPT] [OF <place>, …] [TO '<name>']`, or `BACKUP [LOG]
    /// [FROM n] [TO '<name>']`, from just after `BACKUP`.
    ///
    /// A bare `BACKUP` is the state snapshot (ADR-0094 D1): the routine backup of
    /// a store whose log is bounded. The log is asked for by name, `LOG`, or by
    /// `FROM`, which only a log has.
    pub(in crate::parser) fn backup_statement(&mut self) -> Result<StatementKind> {
        self.advance();
        // `STATE`, `SCRIPT` and `LOG` are contextual, as every word a
        // statement's head adds is: ordinary field names everywhere else.
        let form = if self.eat_word("state") {
            Some(crate::ast::BackupForm::State)
        } else if self.eat_word("script") {
            Some(crate::ast::BackupForm::Script)
        } else if self.eat_word("log") || self.peek_keyword() == Some(Keyword::From) {
            None
        } else {
            Some(crate::ast::BackupForm::State)
        };
        Ok(if let Some(form) = form {
            if self.eat_keyword(Keyword::From) {
                return Err(self.error_here(
                    "the end of the statement; a snapshot or a script is one moment, \
                     and `FROM` names a position in a log",
                ));
            }
            let of = self.backup_of()?;
            if form == crate::ast::BackupForm::State && !of.is_empty() {
                return Err(self.error_here(
                    "`BACKUP SCRIPT OF` for a part; a snapshot carries the store's catalog as \
                     it is kept, so it is taken of the whole store",
                ));
            }
            StatementKind::Backup {
                from: None,
                form,
                to: self.backup_to()?,
                of,
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
            if self.eat_word("of") {
                return Err(self.error_here(
                    "`STATE` or `SCRIPT` before `OF`; a database's log holds its records but \
                     not the definitions of the namespace and database it lives in, so a log \
                     of a part restores nowhere on its own",
                ));
            }
            StatementKind::Backup {
                from,
                form: crate::ast::BackupForm::Log,
                to: self.backup_to()?,
                of: Vec::new(),
            }
        })
    }

    /// `RESTORE SCRIPT FROM '<name>'`, from just after `RESTORE`.
    ///
    /// Only a script: it is the one backup written in names, so it is the one
    /// that can land beside what a live store already holds.
    pub(in crate::parser) fn restore_statement(&mut self) -> Result<StatementKind> {
        if !self.eat_word("script") {
            return Err(self.error_here(
                "`SCRIPT`; a log or a snapshot restores into an empty store with `--restore`",
            ));
        }
        if !self.eat_keyword(Keyword::From) {
            return Err(self.error_here("`FROM` and the file name, quoted"));
        }
        let Some(Token::Str(name)) = self.peek() else {
            return Err(self.error_here("the file name the script is read from, quoted"));
        };
        let from = name.clone();
        self.advance();
        Ok(StatementKind::Restore { from })
    }

    /// The places a partial backup carries — `NAMESPACE prod`, `DATABASE
    /// prod.orders` or the bare `prod.orders` — when the statement names any.
    fn backup_of(&mut self) -> Result<Vec<ReachRef>> {
        if !self.eat_word("of") {
            return Ok(Vec::new());
        }
        let mut places = Vec::new();
        loop {
            match self.reach_ref()? {
                reach @ (ReachRef::Namespace(_) | ReachRef::Database(_)) => places.push(reach),
                _ => {
                    return Err(self.error_here(
                        "a namespace or a database; a backup of the store names no `OF`",
                    ));
                }
            }
            if !self.eat_punct(Punct::Comma) {
                return Ok(places);
            }
        }
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
