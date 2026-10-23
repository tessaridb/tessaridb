//! The joins a read names: `JOIN`, `INNER JOIN`, `ASOF JOIN` and the three
//! outer joins (ADR-0134 D4).

use super::super::Parser;

use crate::ast::{JoinSide, Outer, Source};
use crate::error::{Error, Result};
use crate::token::{Keyword, Punct, Token};

use super::super::side_of;
use crate::ast::FieldPath;

impl Parser<'_> {
    /// The rest of `FROM users JOIN orders ON users.id = orders.user`.
    ///
    /// # Both sides of `ON` are routes into the joined row
    ///
    /// Which is why they are written with the table in front: the row is
    /// `{ users: { … }, orders: { … } }`, so `users.id` is the path it looks
    /// like. They may be written either way round — the parser sorts out which
    /// side is which — because a reader writing the condition is thinking about
    /// the two fields and not about which table the statement named first.
    ///
    /// The root is then stripped, so what the executor holds is a route into a
    /// *record* on each side. That is what lets the right side be probed through
    /// an index, which reads records and knows nothing about a composite.
    ///
    /// A `RIGHT JOIN` is read here as a `LEFT JOIN` with the sides exchanged:
    /// the right side drives, and each row answers under its record's identity.
    pub(super) fn join(&mut self, left: JoinSide, kind: JoinKind) -> Result<Source> {
        let right = self.join_side()?;
        let on = self.span_here();
        self.expect_keyword(Keyword::On, "`ON` and the two fields to match")?;
        let first = self.field_path()?;
        self.expect_punct(Punct::Equals, "`=` between the two sides of the join")?;
        let second = self.field_path()?;

        // The two **names**, not the two tables: `users AS a JOIN users AS b` is
        // one table under two names and reads perfectly, while `users JOIN users`
        // is two names that are one and has no row a reader could address.
        if left.names().contains(&right.name()) {
            return Err(Error::OneSidedJoin {
                name: right.name(),
                span: on.to(self.span_behind()),
            });
        }
        let sides = [left.name(), right.name()];
        let first_side = side_in(&first, &left, &sides)?;
        let second_side = side_in(&second, &left, &sides)?;
        if first_side.0 == second_side.0 {
            return Err(Error::OneSidedJoin {
                name: sides[first_side.0].clone(),
                span: on.to(self.span_behind()),
            });
        }
        let (left_key, right_key) = if first_side.0 == 0 {
            (first_side.1, second_side.1)
        } else {
            (second_side.1, first_side.1)
        };

        // A further `JOIN` joins its side to the rows so far (ADR-0134 D14); a
        // `WHERE` belongs to the whole chain and is read after its last side.
        if kind != JoinKind::Asof
            && let Some(next) = self.next_join_kind()?
        {
            let so_far = Self::joined_source(kind, (left, right), (left_key, right_key), None);
            return self.join(JoinSide::Joined(Box::new(so_far)), next);
        }
        let condition = self
            .eat_keyword(Keyword::Where)
            .then(|| self.condition().map(Box::new))
            .transpose()?;
        Ok(Self::joined_source(
            kind,
            (left, right),
            (left_key, right_key),
            condition,
        ))
    }

    /// The next join of a chain, consumed with its `JOIN`; `ASOF` joins two
    /// sides only.
    fn next_join_kind(&mut self) -> Result<Option<JoinKind>> {
        if let Some(kind) = self.join_kind()? {
            return Ok(Some(kind));
        }
        Ok(self.eat_keyword(Keyword::Join).then_some(JoinKind::Inner))
    }

    /// A parsed join as its source, with a `RIGHT JOIN`'s sides exchanged.
    fn joined_source(
        kind: JoinKind,
        (left, right): (JoinSide, JoinSide),
        (left_key, right_key): (crate::ast::FieldPath, crate::ast::FieldPath),
        condition: Option<Box<crate::ast::Expr>>,
    ) -> Source {
        let (left, right, left_key, right_key, outer) = match kind {
            JoinKind::Inner | JoinKind::Asof => (left, right, left_key, right_key, None),
            JoinKind::Outer(Direction::Left) => {
                (left, right, left_key, right_key, Some(Outer::Left))
            }
            JoinKind::Outer(Direction::Right) => {
                (right, left, right_key, left_key, Some(Outer::Left))
            }
            JoinKind::Outer(Direction::Full) => {
                (left, right, left_key, right_key, Some(Outer::Full))
            }
        };
        Source::Join {
            left: Box::new(left),
            right: Box::new(right),
            left_key,
            right_key,
            asof: kind == JoinKind::Asof,
            outer,
            condition,
        }
    }

    /// The side after `JOIN` — a table, or a read that must name itself.
    pub(super) fn join_side(&mut self) -> Result<JoinSide> {
        if self.peek() == Some(&Token::Punct(Punct::ParenOpen)) {
            let read = self.parenthesised_read()?;
            let Some(alias) = self.alias()? else {
                return Err(self.error_here(
                    "`AS <name>` after the read — a read has no name of its own, and a \
                     row files each side under a name",
                ));
            };
            return Ok(JoinSide::Read {
                read: Box::new(read),
                alias,
            });
        }
        let table = self.table_ref()?;
        let named = self.named_read(&table);
        Ok(super::named::side(table, self.alias()?, named))
    }

    /// `INNER JOIN`, or `LEFT`, `RIGHT` or `FULL` with an optional `OUTER`
    /// before `JOIN` (ADR-0134 D4), consumed with the `JOIN`; `None` when no
    /// such word stands here.
    ///
    /// `INNER` is the bare `JOIN` spelled out, and `OUTER` says nothing the
    /// direction did not — both are accepted because a SQL reader types them.
    pub(super) fn join_kind(&mut self) -> Result<Option<JoinKind>> {
        let kind = if self.eat_word("inner") {
            JoinKind::Inner
        } else if self.eat_word("left") {
            JoinKind::Outer(Direction::Left)
        } else if self.eat_word("right") {
            JoinKind::Outer(Direction::Right)
        } else if self.eat_word("full") {
            JoinKind::Outer(Direction::Full)
        } else {
            return Ok(None);
        };
        if matches!(kind, JoinKind::Outer(_)) {
            self.eat_word("outer");
        }
        self.expect_keyword(Keyword::Join, "`JOIN` after the kind of join")?;
        Ok(Some(kind))
    }
}

/// Which join a read asked for, as written.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum JoinKind {
    /// `JOIN` or `INNER JOIN`.
    Inner,
    /// `ASOF JOIN` (ADR-0088 §4).
    Asof,
    /// `LEFT`, `RIGHT` or `FULL` `[OUTER] JOIN`.
    Outer(Direction),
}

/// Which side an outer join keeps.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Direction {
    Left,
    Right,
    Full,
}

/// Which side of a join a key routes into, and the route within it. A key into
/// the join so far keeps its side's name, since that join's rows are filed
/// under every name it holds (ADR-0134 D14).
fn side_in(key: &FieldPath, left: &JoinSide, sides: &[String; 2]) -> Result<(usize, FieldPath)> {
    if matches!(left, JoinSide::Joined(_))
        && left.names().iter().any(|name| name == key.path.root())
    {
        return Ok((0, key.clone()));
    }
    side_of(key, sides)
}
