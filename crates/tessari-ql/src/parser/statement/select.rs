//! SELECT, its sources and its joins.

use super::Parser;

use crate::ast::{Expr, JoinSide, Name, Projection, Select, Source};
use crate::error::{Error, Result};
use crate::token::{Keyword, Punct, Token};

use super::{NODE_SOURCE, side_of};

impl Parser<'_> {
    /// `..` or `..=`, when a span's bound follows.
    ///
    /// Answers whether the upper bound is **inclusive**, so the two spellings
    /// are read once here rather than compared again at every use.
    pub(super) fn range_bound(&mut self) -> Option<bool> {
        if self.eat_punct(Punct::DotDotEquals) {
            return Some(true);
        }
        if self.eat_punct(Punct::DotDot) {
            return Some(false);
        }
        None
    }

    /// What the `FROM` names, resolved to exactly one access path.
    pub(super) fn select_source(&mut self) -> Result<Source> {
        // `$node` before the table, because it is the one source that is not a
        // name. A parameter is not legal in this position at all — a value
        // cannot say which table to read — so recognising this one takes nothing
        // away from a caller, and a parameter they *supply* called `node` stays
        // theirs, unshadowed, everywhere a value belongs. That is the difference
        // between a source spelled with a sigil and a reserved parameter name,
        // which ADR-0018's amendment rejects for exactly the shadowing it would
        // have introduced.
        if matches!(self.peek(), Some(Token::Parameter(name)) if name == NODE_SOURCE) {
            self.advance();
            return Ok(Source::Node);
        }
        if self.peek() == Some(&Token::Punct(Punct::ParenOpen)) {
            return self.subquery_source();
        }
        if self.eat_keyword(Keyword::Search) {
            return self.search_source();
        }
        let table = self.table_ref()?;
        if self.peek() == Some(&Token::Punct(Punct::Colon)) {
            let record = self.record_target_after(table)?;
            // `events:1000..2000` reads as a span, and `events:1000` as one
            // record, decided by the two dots and nothing else. The identity is
            // parsed first either way, so a span costs no lookahead and a
            // record's grammar does not change.
            if let Some(inclusive) = self.range_bound() {
                // The table is not written again. `events:1000..events:2000`
                // would let somebody name two tables in one span, and there is
                // no answer to that question — so the upper bound is an
                // identity and the table is the one the lower bound named.
                let at = self.span_here();
                let upper = self.record_id(at)?;
                return Ok(Source::Range {
                    span: record.span.to(self.span_behind()),
                    table: record.table,
                    lower: record.id,
                    upper,
                    inclusive,
                });
            }
            return Ok(match self.arrow() {
                Some(direction) => self.traversal(record, direction)?,
                None => Source::Record(record),
            });
        }
        let alias = self.alias()?;
        // `ASOF` is contextual: it only means something directly before `JOIN`.
        let asof = self.eat_word("asof");
        if asof && self.peek_keyword() != Some(Keyword::Join) {
            return Err(self.error_here("`JOIN` after `ASOF`"));
        }
        if self.eat_keyword(Keyword::Join) {
            return self.join(JoinSide::Table { table, alias }, asof);
        }
        if alias.is_some() {
            return Err(self.aliased_without_a_join());
        }
        if self.eat_keyword(Keyword::Where) {
            return Ok(Source::Where {
                table,
                condition: Box::new(self.condition()?),
            });
        }
        Ok(Source::Table(table))
    }

    /// `AS <name>`, consumed if it is there.
    ///
    /// A failure here is returned rather than swallowed as "no alias": `AS 3`
    /// would otherwise fall through and be reported many tokens later, pointing
    /// at whatever the parser tripped over next instead of at the name.
    pub(super) fn alias(&mut self) -> Result<Option<Name>> {
        if !self.eat_keyword(Keyword::As) {
            return Ok(None);
        }
        Ok(Some(self.name()?))
    }

    /// A name was given to a source that is not a side of anything.
    ///
    /// Accepting it and ignoring it would be the quieter choice and the wrong
    /// one: a reader who wrote a name expects to be able to use it, and a read
    /// with one source answers its records under no name at all.
    pub(super) fn aliased_without_a_join(&mut self) -> Error {
        self.error_here("`JOIN` — a name given with `AS` names one side of a join")
    }

    /// `FROM ( <read> )`, on its own or as the left side of a join.
    ///
    /// The inner read must state a `LIMIT`. It is materialised — there is no
    /// index to walk and no bound to push into it — so a source that could grow
    /// without limit is refused rather than cut at a number nobody wrote. A
    /// silently truncated source answers a different question from the one that
    /// was asked and looks exactly like a complete one.
    pub(super) fn subquery_source(&mut self) -> Result<Source> {
        let read = self.parenthesised_read()?;
        let Some(alias) = self.alias()? else {
            if self.peek_keyword() == Some(Keyword::Join) {
                return Err(self.error_here(
                    "`AS <name>` before `JOIN` — a read has no name of its own, and a \
                     row files each side under a name",
                ));
            }
            return Ok(Source::Subquery {
                read: Box::new(read),
                condition: self.materialised_condition()?,
            });
        };
        if !self.eat_keyword(Keyword::Join) {
            return Err(self.aliased_without_a_join());
        }
        self.join(
            JoinSide::Read {
                read: Box::new(read),
                alias,
            },
            false,
        )
    }

    /// `WHERE …` after a materialised source, consumed if it is there.
    ///
    /// The same clause `FROM t WHERE c` carries, in the one other position a
    /// source can stand. Its records are already in hand, so the condition
    /// narrows them rather than choosing an access path.
    pub(super) fn materialised_condition(&mut self) -> Result<Option<Box<Expr>>> {
        if !self.eat_keyword(Keyword::Where) {
            return Ok(None);
        }
        Ok(Some(Box::new(self.condition()?)))
    }

    /// `( SELECT … LIMIT n )` — the read a source materialises.
    pub(super) fn parenthesised_read(&mut self) -> Result<Select> {
        self.expect_punct(Punct::ParenOpen, "`(` and the read to materialise")?;
        if self.peek_keyword() != Some(Keyword::Select) {
            return Err(self.error_here("`SELECT` — a source in parentheses is a read"));
        }
        let read = self.select_statement()?;
        if read.limit.is_none() && read.limit_parameter.is_none() {
            return Err(self.error_here(
                "`LIMIT n` on the inner read — a materialised source states how much \
                 it may hold, so that a truncated answer is never mistaken for a whole one",
            ));
        }
        self.expect_punct(Punct::ParenClose, "`)` after the materialised read")?;
        Ok(read)
    }

    /// `SELECT <projection> FROM …`, resolving to exactly one access path.
    pub(in crate::parser) fn select_statement(&mut self) -> Result<Select> {
        let start = self.span_here();
        self.advance();
        let projection = self.projection()?;
        // Beside the projection rather than among the clauses after `FROM`,
        // because it says what the star contributes and not what the read does.
        let omit = self.omit_paths(&projection)?;
        self.expect_keyword(Keyword::From, "`FROM` and what to read")?;
        // Between `FROM` and the source because that is what it qualifies: how
        // many of them there are to answer with, said before the thing itself.
        let only = self.eat_keyword(Keyword::Only).then(|| self.span_behind());

        let from = self.select_source()?;
        // Written in the order it is applied: references are followed before
        // anything groups, projects or sorts, so the clause sits before them.
        // The grammar keeps clause order and application order the same on
        // purpose — see `START` before `LIMIT` below.
        // Before everything else that shapes the answer, because it decides
        // which records there are: the newest per key, after the condition.
        let latest = self.latest_by()?;
        let fetch = self.fetch_paths()?;
        // After the fetch and before everything that counts records, which is
        // where it is applied: the split is what decides how many there are.
        let split = self.split_path()?;
        let group = self.group_by()?;
        // Straight after the grouping it completes: the windows it adds are
        // groups, and everything after this clause treats them as groups.
        let fill = self.fill()?;
        let (order, fusion) = self.order_by()?;
        // After the order, because the order is what it resumes: the anchor is
        // the last record of the page before, and "after" is a position in the
        // sequence the clause above just named. Before `START`, which it is also
        // refused beside — both say where the page begins.
        let after = self.after_anchor()?;
        // `START` before `LIMIT`, because that is the order they are applied in
        // and a grammar that let them be written either way would suggest they
        // commute.
        let (skip, start_parameter) = self.bound_count("start")?;
        let (limit, limit_parameter) = self.bound_count("limit")?;
        // Last, because it qualifies the whole read rather than any one clause,
        // and contextual like the rest: a field called `approximate` stays a
        // field.
        let approximate = self.approximation()?;
        // Beside `APPROXIMATE` because it qualifies the read the same way — both
        // say something about how the answer may be produced — and before
        // `USING`, which is an assertion about what the read then did.
        let lift_scan_guard = self.scan_guard()?;
        // After everything, because it is an assertion *about* the read rather
        // than part of it — nothing below the parser reads it to decide
        // anything. Contextual like the rest, so a field called `using` stays a
        // field.
        let using = self.using()?;
        // After `USING`, so the tail reads in the order a statement is thought
        // about: what to read, how much of it, what it should have done, and how
        // long it may take doing it.
        let timeout = self.timeout()?;
        // Last of all. It qualifies the whole read the way `USING` and `TIMEOUT`
        // do, and a reader who has taken in the question is then told which
        // state answered it.
        let version = self.version()?;
        // After `VERSION`, because it is the clause that may disagree with it:
        // a read naming one exact point in history has no room for a tolerance
        // about how old that point is.
        let staleness = self.staleness()?;
        if let (Some(_), Some(bound)) = (version.as_ref(), staleness.as_ref()) {
            return Err(Error::StalenessBesideAVersion { span: bound.span });
        }
        // Last, and deliberately not beside `STALENESS` even though the two are
        // the pair a reader will compare. They answer different questions —
        // *how old may the copy be* against *which node may answer at all* — so
        // there is no pairing rule between them to enforce here: naming both is
        // legal and the read is answered only where both hold.
        let answered_by = self.answered_by()?;
        crate::parser::shape::check_grouping(&projection, &group)?;
        crate::parser::shape::check_fold_positions(&from, &group, &order)?;
        // A fused order ranks records, and a grouped read's rows are groups; a
        // cursor resumes a position in one total order, which a fused order is
        // not until every branch has been ranked. Both refused by name.
        if let Some(fused) = &fusion {
            for (clause, present) in [("GROUP BY", !group.is_empty()), ("AFTER", after.is_some())] {
                if present {
                    return Err(Error::UnexpectedToken {
                        expected: "a read that is not grouped or resumed — a fused order ranks \
                                   the records themselves, in one pass",
                        found: clause.to_owned(),
                        span: fused.span,
                    });
                }
            }
        }
        crate::parser::shape::check_cursor(
            &from,
            after.as_deref(),
            skip,
            [
                ("GROUP BY", !group.is_empty()),
                ("FETCH", !fetch.is_empty()),
                ("SPLIT ON", split.is_some()),
            ],
        )?;
        // Where `[*]` may stand. A condition admits one on the left of a
        // comparison and a projection admits one as a whole projected value; a
        // key and an ordering do not yet, and each is refused by name rather
        // than by a stray-token message.
        if let Projection::Values { values, .. } = &projection {
            for value in values {
                crate::parser::shape::check_projected(&value.value)?;
            }
        }
        for key in &group {
            crate::parser::shape::no_several(key)?;
        }
        for ordering in &order {
            crate::parser::shape::no_several(&ordering.key)?;
        }
        for route in &fetch {
            crate::parser::shape::no_several_path(route)?;
        }
        match &from {
            Source::Where { condition, .. } => crate::parser::shape::check_several(condition)?,
            Source::Join { condition, .. } => {
                if let Some(condition) = condition {
                    crate::parser::shape::check_several(condition)?;
                }
            }
            Source::Subquery {
                condition: Some(condition),
                ..
            } => crate::parser::shape::check_several(condition)?,
            // A ranking across tables has no key order to seek in and no one
            // table's history, so a cursor, a version, a fused order and a
            // fetch are refused rather than given a meaning here.
            Source::Search { condition, .. } => {
                if let Some(condition) = condition {
                    crate::parser::shape::check_several(condition)?;
                }
                if after.is_some()
                    || version.is_some()
                    || fusion.is_some()
                    || !fetch.is_empty()
                    || split.is_some()
                    || latest.is_some()
                    || timeout.is_some()
                {
                    return Err(Error::Unsupported {
                        feature: "`AFTER`, `VERSION`, `ORDER BY FUSE`, `FETCH`, `SPLIT ON`, \
                                  `LATEST BY` or `TIMEOUT` over a search",
                        span: start.to(self.span_behind()),
                    });
                }
            }
            Source::Node
            | Source::Record(_)
            | Source::Table(_)
            | Source::Range { .. }
            | Source::Traverse { .. }
            | Source::Subquery { .. } => {}
        }
        Ok(Select {
            projection,
            omit,
            from,
            only,
            fetch,
            split,
            group,
            fill,
            latest,
            order,
            fusion,
            after,
            approximate,
            lift_scan_guard,
            start: skip,
            limit,
            using,
            timeout,
            version,
            staleness,
            answered_by,
            start_parameter,
            limit_parameter,
            span: start.to(self.span_behind()),
        })
    }

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
    pub(super) fn join(&mut self, left: JoinSide, asof: bool) -> Result<Source> {
        let right = self.join_side()?;
        let on = self.span_here();
        self.expect_keyword(Keyword::On, "`ON` and the two fields to match")?;
        let first = self.field_path()?;
        self.expect_punct(Punct::Equals, "`=` between the two sides of the join")?;
        let second = self.field_path()?;

        // The two **names**, not the two tables: `users AS a JOIN users AS b` is
        // one table under two names and reads perfectly, while `users JOIN users`
        // is two names that are one and has no row a reader could address.
        if left.name() == right.name() {
            return Err(Error::OneSidedJoin {
                name: left.name().to_owned(),
                span: on.to(self.span_behind()),
            });
        }
        let sides = [&left, &right].map(|side| side.name().to_owned());
        let first_side = side_of(&first, &sides)?;
        let second_side = side_of(&second, &sides)?;
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

        let condition = self
            .eat_keyword(Keyword::Where)
            .then(|| self.condition().map(Box::new))
            .transpose()?;
        Ok(Source::Join {
            left: Box::new(left),
            right: Box::new(right),
            left_key,
            right_key,
            asof,
            condition,
        })
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
        Ok(JoinSide::Table {
            table,
            alias: self.alias()?,
        })
    }
}
