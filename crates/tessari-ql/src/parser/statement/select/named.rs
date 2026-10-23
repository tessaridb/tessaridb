//! `WITH name AS (<read>) [, …] <read>` — named reads (ADR-0134 D12).
//!
//! A name stands for its read wherever a table may stand in a `FROM` or a join
//! side, and the parser puts the read there: the statement that runs is the one
//! a reader would have written with the read in parentheses, so the ceiling, the
//! note when it is reached, and every rule of a materialised source are that
//! source's own rather than a second set kept beside them.

use super::Parser;

use crate::ast::{JoinSide, Name, Select, StatementKind, TableRef};
use crate::error::Result;
use crate::token::{Keyword, Punct};

impl Parser<'_> {
    /// Everything after `WITH`: the named reads, then the read they are for.
    pub(in crate::parser) fn with_statement(&mut self) -> Result<StatementKind> {
        let outer = self.named_reads.len();
        loop {
            let at = self.position;
            let name = self.name()?;
            if self
                .named_reads
                .iter()
                .skip(outer)
                .any(|(given, _)| given == &name.text)
            {
                return Err(self.error_at(
                    at,
                    "a name not already given in this `WITH` — a read named twice \
                     would leave the second one unreachable",
                ));
            }
            self.expect_keyword(Keyword::As, "`AS` and the read the name stands for")?;
            let read = self.parenthesised_read()?;
            self.named_reads.push((name.text, read));
            if !self.eat_punct(Punct::Comma) {
                break;
            }
        }
        if self.peek_keyword() != Some(Keyword::Select) {
            return Err(self.error_here("`SELECT` — the read the named reads are for"));
        }
        let read = self.select_statement();
        self.named_reads.truncate(outer);
        Ok(StatementKind::Select(Box::new(read?)))
    }

    /// The read a name in this statement's `WITH` stands for, when the table
    /// named here is one. A qualified name is a table in another database and
    /// never a named read.
    pub(super) fn named_read(&self, table: &TableRef) -> Option<Select> {
        if table.database.is_some() {
            return None;
        }
        self.named_reads
            .iter()
            .rev()
            .find(|(name, _)| name == &table.name.text)
            .map(|(_, read)| read.clone())
    }
}

/// One side of a join: the named read when the table names one, filed under the
/// alias or the name itself, and the table otherwise.
pub(super) fn side(table: TableRef, alias: Option<Name>, named: Option<Select>) -> JoinSide {
    match named {
        Some(read) => JoinSide::Read {
            read: Box::new(read),
            alias: alias.unwrap_or(table.name),
        },
        None => JoinSide::Table { table, alias },
    }
}
