//! `LIST`, `HASH`, `MEMBERS` and `SCORES` — the writes that change a container
//! held under one space key (G079 C6).
//!
//! # Every word here is contextual
//!
//! `list`, `hash`, `members` and `scores` are field names in exactly the
//! schemas that want these statements, so none is reserved. Each heads a
//! statement only, where nothing else can stand, and each is followed by its
//! operation word before anything is read.
//!
//! A field, an index or a member before an `=` is read as an [operand] rather
//! than an expression, because an expression there would read the `=` as a
//! comparison.
//!
//! [operand]: Parser::operand

use super::Parser;
use crate::ast::{
    Expr, ListEnd, Presence, RangeEnd, ScoreCompare, ScoreRange, StatementKind, StructureOp,
};
use crate::error::Result;
use crate::token::{Keyword, Punct, Token};

impl Parser<'_> {
    /// `LIST <op> …`, the word `LIST` consumed.
    pub(super) fn list_statement(&mut self) -> Result<StatementKind> {
        let op = if self.eat_word("push") {
            let target = self.record_target()?;
            let end = self.list_end()?;
            let values = self.values()?;
            let if_present = if self.eat_keyword(Keyword::If) {
                self.expect_word("present", "`PRESENT` — push only onto a list already there")?;
                true
            } else {
                false
            };
            (
                target,
                StructureOp::ListPush {
                    end,
                    values,
                    if_present,
                },
            )
        } else if self.eat_word("pop") {
            let target = self.record_target()?;
            let end = self.list_end()?;
            (
                target,
                StructureOp::ListPop {
                    end,
                    limit: self.limit()?,
                },
            )
        } else if self.eat_keyword(Keyword::Set) {
            let target = self.record_target()?;
            self.expect_word("at", "`AT` and the index the element is at")?;
            let index = self.operand()?;
            self.expect_punct(Punct::Equals, "`=` and the new element")?;
            (
                target,
                StructureOp::ListSet {
                    index,
                    value: self.expression()?,
                },
            )
        } else if self.eat_word("trim") {
            let target = self.record_target()?;
            self.expect_keyword(Keyword::From, "`FROM` and the first index kept")?;
            let start = self.expression()?;
            self.expect_keyword(Keyword::To, "`TO` and the last index kept")?;
            (
                target,
                StructureOp::ListTrim {
                    start,
                    stop: self.expression()?,
                },
            )
        } else if self.eat_word("remove") {
            let target = self.record_target()?;
            let value = self.expression()?;
            let limit = self.limit()?;
            let from_back = if self.eat_keyword(Keyword::From) {
                self.expect_word("back", "`BACK` — count from the back")?;
                true
            } else {
                false
            };
            (
                target,
                StructureOp::ListRemove {
                    value,
                    limit,
                    from_back,
                },
            )
        } else if self.eat_keyword(Keyword::Insert) {
            let target = self.record_target()?;
            let value = self.expression()?;
            let after = if self.eat_word("after") {
                true
            } else if self.eat_word("before") {
                false
            } else {
                return Err(self.error_here("`BEFORE` or `AFTER` and the element it goes beside"));
            };
            (
                target,
                StructureOp::ListInsert {
                    value,
                    after,
                    pivot: self.expression()?,
                },
            )
        } else if self.eat_word("move") {
            let target = self.record_target()?;
            let from = self.list_end()?;
            self.expect_keyword(Keyword::To, "`TO` and the list it goes to")?;
            let to = self.record_target()?;
            (
                target,
                StructureOp::ListMove {
                    from,
                    to,
                    to_end: self.list_end()?,
                },
            )
        } else {
            return Err(
                self.error_here("`PUSH`, `POP`, `SET`, `TRIM`, `REMOVE`, `INSERT` or `MOVE`")
            );
        };
        Ok(StatementKind::Structure {
            target: op.0,
            op: op.1,
        })
    }

    /// `HASH <op> …`, the word `HASH` consumed.
    pub(super) fn hash_statement(&mut self) -> Result<StatementKind> {
        let op = if self.eat_keyword(Keyword::Set) {
            let target = self.record_target()?;
            let fields = self.pairs("`=` and the field's value")?;
            let if_absent = if self.eat_keyword(Keyword::If) {
                self.expect_word("absent", "`ABSENT` — set only fields not yet there")?;
                true
            } else {
                false
            };
            (target, StructureOp::HashSet { fields, if_absent })
        } else if self.eat_word("remove") {
            let target = self.record_target()?;
            (
                target,
                StructureOp::HashRemove {
                    fields: self.values()?,
                },
            )
        } else if self.eat_word("incr") {
            let target = self.record_target()?;
            let field = self.expression()?;
            self.expect_word("by", "`BY` and how much to add")?;
            (
                target,
                StructureOp::HashIncr {
                    field,
                    by: self.expression()?,
                },
            )
        } else {
            return Err(self.error_here("`SET`, `REMOVE` or `INCR`"));
        };
        Ok(StatementKind::Structure {
            target: op.0,
            op: op.1,
        })
    }

    /// `MEMBERS <op> …`, the word `MEMBERS` consumed.
    pub(super) fn members_statement(&mut self) -> Result<StatementKind> {
        let op = if self.eat_word("add") {
            let target = self.record_target()?;
            (
                target,
                StructureOp::MembersAdd {
                    values: self.values()?,
                },
            )
        } else if self.eat_word("remove") {
            let target = self.record_target()?;
            (
                target,
                StructureOp::MembersRemove {
                    values: self.values()?,
                },
            )
        } else if self.eat_word("pop") {
            let target = self.record_target()?;
            (
                target,
                StructureOp::MembersPop {
                    limit: self.limit()?,
                },
            )
        } else if self.eat_word("move") {
            let target = self.record_target()?;
            let value = self.expression()?;
            self.expect_keyword(Keyword::To, "`TO` and the set it goes to")?;
            (
                target,
                StructureOp::MembersMove {
                    value,
                    to: self.record_target()?,
                },
            )
        } else {
            return Err(self.error_here("`ADD`, `REMOVE`, `POP` or `MOVE`"));
        };
        Ok(StatementKind::Structure {
            target: op.0,
            op: op.1,
        })
    }

    /// `SCORES <op> …`, the word `SCORES` consumed.
    pub(super) fn scores_statement(&mut self) -> Result<StatementKind> {
        let op = if self.eat_word("add") {
            let target = self.record_target()?;
            let scores = self.pairs("`=` and the member's score")?;
            let (mut presence, mut compare) = (None, None);
            while self.eat_keyword(Keyword::If) {
                if presence.is_none() && self.eat_word("absent") {
                    presence = Some(Presence::Absent);
                } else if presence.is_none() && self.eat_word("present") {
                    presence = Some(Presence::Present);
                } else if compare.is_none() && self.eat_word("greater") {
                    compare = Some(ScoreCompare::Greater);
                } else if compare.is_none() && self.eat_word("less") {
                    compare = Some(ScoreCompare::Less);
                } else {
                    return Err(
                        self.error_here("`ABSENT`, `PRESENT`, `GREATER` or `LESS`, each kind once")
                    );
                }
            }
            let changed = self.eat_word("changed");
            (
                target,
                StructureOp::ScoresAdd {
                    scores,
                    presence,
                    compare,
                    changed,
                },
            )
        } else if self.eat_word("incr") {
            let target = self.record_target()?;
            let member = self.expression()?;
            self.expect_word("by", "`BY` and how much to add")?;
            (
                target,
                StructureOp::ScoresIncr {
                    member,
                    by: self.expression()?,
                },
            )
        } else if self.eat_word("remove") {
            let target = self.record_target()?;
            let op = match self.score_range()? {
                Some(range) => StructureOp::ScoresRemoveRange { range },
                None => StructureOp::ScoresRemove {
                    members: self.values()?,
                },
            };
            (target, op)
        } else if self.eat_word("pop") {
            let target = self.record_target()?;
            let highest = if self.eat_word("highest") {
                true
            } else if self.eat_word("lowest") {
                false
            } else {
                return Err(self.error_here("`LOWEST` or `HIGHEST`"));
            };
            (
                target,
                StructureOp::ScoresPop {
                    highest,
                    limit: self.limit()?,
                },
            )
        } else {
            return Err(self.error_here("`ADD`, `INCR`, `REMOVE` or `POP`"));
        };
        Ok(StatementKind::Structure {
            target: op.0,
            op: op.1,
        })
    }

    /// `RANK|SCORE|MEMBER FROM … TO …`, when one of the three words stands
    /// before a `FROM` — anywhere else the word is a member like any other.
    fn score_range(&mut self) -> Result<Option<ScoreRange>> {
        let by = ["rank", "score", "member"]
            .into_iter()
            .find(|word| self.peek_word(word));
        let Some(by) =
            by.filter(|_| matches!(self.peek_ahead(1), Some(Token::Keyword(Keyword::From))))
        else {
            return Ok(None);
        };
        self.eat_word(by);
        self.expect_keyword(Keyword::From, "`FROM` and where the range starts")?;
        if by == "rank" {
            let start = self.expression()?;
            self.expect_keyword(Keyword::To, "`TO` and the last position")?;
            return Ok(Some(ScoreRange::Rank {
                start,
                stop: self.expression()?,
            }));
        }
        let low = self.range_end()?;
        self.expect_keyword(Keyword::To, "`TO` and where the range ends")?;
        let high = self.range_end()?;
        Ok(Some(if by == "score" {
            ScoreRange::Score { low, high }
        } else {
            ScoreRange::Member { low, high }
        }))
    }

    /// One end of a range and whether it is left out.
    fn range_end(&mut self) -> Result<RangeEnd> {
        let value = self.expression()?;
        Ok(RangeEnd {
            value,
            exclusive: self.eat_word("exclusive"),
        })
    }

    /// `FRONT` or `BACK`.
    fn list_end(&mut self) -> Result<ListEnd> {
        if self.eat_word("front") {
            Ok(ListEnd::Front)
        } else if self.eat_word("back") {
            Ok(ListEnd::Back)
        } else {
            Err(self.error_here("`FRONT` or `BACK`"))
        }
    }

    /// One or more values, separated by commas.
    fn values(&mut self) -> Result<Vec<Expr>> {
        let mut values = vec![self.expression()?];
        while self.eat_punct(Punct::Comma) {
            values.push(self.expression()?);
        }
        Ok(values)
    }

    /// One or more `name = value`, separated by commas.
    fn pairs(&mut self, expected: &'static str) -> Result<Vec<(Expr, Expr)>> {
        let mut pairs = Vec::new();
        loop {
            let name = self.operand()?;
            self.expect_punct(Punct::Equals, expected)?;
            pairs.push((name, self.expression()?));
            if !self.eat_punct(Punct::Comma) {
                return Ok(pairs);
            }
        }
    }

    /// An optional `LIMIT n`.
    fn limit(&mut self) -> Result<Option<Expr>> {
        if self.eat_word("limit") {
            Ok(Some(self.expression()?))
        } else {
            Ok(None)
        }
    }
}
