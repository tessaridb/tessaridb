//! The clauses that shape a read rather than choose what it reads.
//!
//! `GROUP BY`, `ORDER BY`, `START` and `LIMIT`, and the one rule that binds
//! them: a grouped read may answer only with its keys and its folds.
//!
//! The words are **contextual**, not reserved. Nothing else can stand in the
//! positions they appear in, so nothing is ambiguous — and reserving `order`,
//! `by`, `group`, `limit`, `start`, `asc` or `desc` would take seven perfectly
//! good names away from data that already exists. This language has a rule
//! about that.

mod checks;
mod guards;

use super::Parser;
use crate::ast::{Expr, ExprKind, FieldPath, Fusion, Ordering, Projection, RecordTarget};
use crate::error::{Error, Result};
use crate::token::{Keyword, Punct, Token};
pub(crate) use checks::{
    check_cursor, check_fold_positions, check_grouping, check_projected, check_several, children,
    no_fold, no_several,
};

impl Parser<'_> {
    /// `GROUP BY city, address.country`, when it is there.
    pub(super) fn group_by(&mut self) -> Result<Vec<Expr>> {
        if !self.eat_word("group") {
            return Ok(Vec::new());
        }
        if !self.eat_word("by") {
            return Err(self.error_here("`BY` after `GROUP`"));
        }
        // Read in the condition position, so a bare name is a route into the
        // record — the reading `WHERE` and `ORDER BY` already give it — and an
        // expression works, which is what makes a window sayable.
        let mut keys = vec![self.condition()?];
        while self.eat_punct(Punct::Comma) {
            keys.push(self.condition()?);
        }
        Ok(keys)
    }

    /// `FILL PREVIOUS FROM <start> TO <end>`, when it is there.
    ///
    /// `PREVIOUS` and `LINEAR` are words; anything else is an expression the
    /// empty windows answer with, `NULL` included. The range is required here
    /// rather than defaulted to the data's extent, because the extent is exactly
    /// what the statement did not say.
    pub(super) fn fill(&mut self) -> Result<Option<crate::ast::Fill>> {
        let span = self.span_here();
        if !self.eat_word("fill") {
            return Ok(None);
        }
        let mode = if self.eat_word("previous") {
            crate::ast::FillMode::Previous
        } else if self.eat_word("linear") {
            crate::ast::FillMode::Linear
        } else {
            crate::ast::FillMode::Value(self.condition()?)
        };
        if !self.eat_keyword(Keyword::From) {
            return Err(self.error_here("`FROM` and the first instant the windows cover"));
        }
        let from = self.condition()?;
        if !self.eat_keyword(Keyword::To) {
            return Err(self.error_here("`TO` and the instant the windows stop before"));
        }
        let to = self.condition()?;
        Ok(Some(crate::ast::Fill {
            mode,
            from,
            to,
            span,
        }))
    }

    /// `FETCH author, meta.editor`, when it is there.
    ///
    /// `fetch` is a **contextual** word and not a reserved one, the same
    /// decision `ORDER`, `GROUP`, `START` and `LIMIT` took: a field called
    /// `fetch` keeps working, and a language that takes a common noun away from
    /// its users to buy a clause has made a poor trade.
    pub(super) fn fetch_paths(&mut self) -> Result<Vec<FieldPath>> {
        if !self.eat_word("fetch") {
            return Ok(Vec::new());
        }
        let mut routes = vec![self.field_path()?];
        while self.eat_punct(Punct::Comma) {
            routes.push(self.field_path()?);
        }
        Ok(routes)
    }

    /// `SPLIT ON tags` after the `FETCH`, when it is there.
    ///
    /// Written where it is applied, like every other clause here. Contextual, so
    /// a field or a table called `split` still works — the position it stands in
    /// holds clause words and never a name, which is the whole difference
    /// between this word and `ONLY`.
    ///
    /// `ON` is required rather than optional. `SPLIT tags` would read as a verb
    /// taking an object, and what the clause does is name the route the rows
    /// come *from*.
    pub(super) fn split_path(&mut self) -> Result<Option<FieldPath>> {
        if !self.eat_word("split") {
            return Ok(None);
        }
        self.expect_keyword(Keyword::On, "`ON` and the route to open")?;
        Ok(Some(self.field_path()?))
    }

    /// `OMIT embedding, address.postcode` after the projection, when it is
    /// there.
    ///
    /// Written next to the `*` it subtracts from rather than down among the
    /// clauses after `FROM`, because it says what the star does and not what the
    /// read does. Contextual like every other clause word here: a field called
    /// `omit` is still a field, and there is no ambiguity to resolve because a
    /// projection reading it as a value has already consumed it by the time this
    /// is asked.
    pub(super) fn omit_paths(&mut self, projection: &Projection) -> Result<Vec<FieldPath>> {
        if !self.peek_word("omit") {
            return Ok(Vec::new());
        }
        // Refused here rather than accepted and ignored: with nothing to
        // subtract from, the clause is either a mistake about what the read
        // answers with or a request to drop a value the author wrote out by
        // name, and both deserve to be said rather than silently dropped.
        if !projection.stars() {
            return Err(self.error_here(
                "a `*` for `OMIT` to subtract from — a value written out by name \
                 was asked for on purpose",
            ));
        }
        self.advance();
        let mut routes = vec![self.omitted_path()?];
        while self.eat_punct(Punct::Comma) {
            routes.push(self.omitted_path()?);
        }
        Ok(routes)
    }

    /// One route `OMIT` may name: fields all the way down, never a position.
    ///
    /// `OMIT tags[0]` would renumber everything after it, so what the answer
    /// held at position one would depend on what was left out — a different
    /// question from the one `OMIT` is for, and refused rather than guessed at.
    fn omitted_path(&mut self) -> Result<FieldPath> {
        let route = self.field_path()?;
        if route
            .path
            .steps()
            .iter()
            .any(|step| !matches!(step, tessari_types::Step::Field(_)))
        {
            return Err(Error::UnexpectedToken {
                expected: "a route of field names — `OMIT` cannot leave out a \
                           position, because the rest would renumber",
                found: route.path.to_string(),
                span: route.span,
            });
        }
        Ok(route)
    }

    /// `ORDER BY name, address.city DESC`, when it is there.
    ///
    /// Keys are read in the condition position, so a bare name is a route into
    /// the record — the same reading a `WHERE` gives it, and the same one a
    /// projection gives it.
    pub(super) fn order_by(&mut self) -> Result<(Vec<Ordering>, Option<Fusion>)> {
        if !self.eat_word("order") {
            return Ok((Vec::new(), None));
        }
        if !self.eat_word("by") {
            return Err(self.error_here("`BY` after `ORDER`"));
        }
        if let Some((branches, fusion)) = self.fused_order()? {
            return Ok((branches, Some(fusion)));
        }
        let mut keys = vec![self.ordering()?];
        while self.eat_punct(Punct::Comma) {
            keys.push(self.ordering()?);
        }
        Ok((keys, None))
    }

    pub(super) fn ordering(&mut self) -> Result<Ordering> {
        let key = self.condition()?;
        // `ASC` is accepted and means nothing, because a reader who writes it is
        // saying what they mean and a grammar that refused would be pedantry.
        let descending = if self.eat_word("desc") {
            true
        } else {
            self.eat_word("asc");
            false
        };
        Ok(Ordering { key, descending })
    }

    /// `AFTER users:1042` after the order, when it is there.
    ///
    /// Contextual, like every clause word here but `ONLY`: a field or a table
    /// called `after` is still one, because the position this stands in holds
    /// clause words and never a name.
    ///
    /// The anchor is written as a record identity — table and all — rather than
    /// as a bare id. It is the spelling every identity in this language already
    /// has, it is exactly what the answer handed back, and carrying the table
    /// is what lets a cursor from another page of another table be refused
    /// instead of silently paging by an identity that happens to compare.
    pub(super) fn after_anchor(&mut self) -> Result<Option<Box<RecordTarget>>> {
        if !self.eat_word("after") {
            return Ok(None);
        }
        let table = self.table_ref()?;
        Ok(Some(Box::new(self.record_target_after(table)?)))
    }

    /// Whether the word one token ahead opens the next clause rather than being
    /// a name belonging to the clause being parsed.
    ///
    /// `USING INDEX by_email` takes a name, and every clause word in this
    /// grammar is contextual — so `USING index TIMEOUT 5s` looks exactly like
    /// `USING INDEX timeout` followed by a stray duration, and the first reading
    /// swallows the next clause. Reserving `timeout` would settle it and would
    /// also take the word away from anyone with an index called `timeout`, which
    /// is the trade this language has already refused seven times.
    ///
    /// So it is settled by what follows instead: `timeout` is a clause only when
    /// a duration comes after it, and a name in every other position. Both
    /// readings stay sayable and neither is guessed at.
    fn opens_the_next_clause(&self) -> bool {
        let Some(Token::Ident(word)) = self.peek_ahead(1) else {
            return false;
        };
        word.eq_ignore_ascii_case("timeout")
            && matches!(self.peek_ahead(2), Some(Token::Duration(_)))
    }
}

/// Whether this expression holds a fold anywhere inside it.
fn holds_a_fold(expr: &Expr) -> bool {
    if matches!(expr.kind, ExprKind::Fold { .. }) {
        return true;
    }
    children(expr).into_iter().any(holds_a_fold)
}

/// Refuse a route reaching several values where a bare route is written.
///
/// An index's fields, a `FETCH` route, a join key: each would need the rule its
/// own task will give it.
pub(super) fn no_several_path(field: &FieldPath) -> Result<()> {
    if field.path.is_several() {
        return Err(Error::SeveralOutsideAComparison { span: field.span });
    }
    Ok(())
}
