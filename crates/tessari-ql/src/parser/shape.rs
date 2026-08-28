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

use tessari_types::Number;

use super::Parser;
use crate::ast::{
    DeleteBound, Expr, ExprKind, FieldPath, Ordering, Projection, RecordTarget, Source, Timeout,
    Using,
};
use crate::error::{Error, Result};
use crate::token::{Keyword, Punct, Span, Token};

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
    pub(super) fn order_by(&mut self) -> Result<Vec<Ordering>> {
        if !self.eat_word("order") {
            return Ok(Vec::new());
        }
        if !self.eat_word("by") {
            return Err(self.error_here("`BY` after `ORDER`"));
        }
        let mut keys = vec![self.ordering()?];
        while self.eat_punct(Punct::Comma) {
            keys.push(self.ordering()?);
        }
        Ok(keys)
    }

    fn ordering(&mut self) -> Result<Ordering> {
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

    /// `LIMIT 10` or `START 20`, when it is there.
    pub(super) fn bound(&mut self, word: &str) -> Result<Option<u64>> {
        if !self.eat_word(word) {
            return Ok(None);
        }
        let expected = "a whole number";
        let Some(Token::Number(Number::Integer(count))) = self.peek() else {
            return Err(self.error_here(expected));
        };
        let count = u64::try_from(*count).map_err(|_| self.error_here(expected))?;
        self.advance();
        Ok(Some(count))
    }

    /// `USING <path>` or `USING INDEX <name>`, when it is there.
    ///
    /// The path word is taken as written and is **not** checked here. The set of
    /// words belongs to the store that reports them, and a copy of it in the
    /// grammar would be a second vocabulary of exactly the kind one plan
    /// structure exists to remove — so an unrecognised word is refused where the
    /// words live, before the read runs, naming the ones that exist.
    pub(super) fn using(&mut self) -> Result<Option<Using>> {
        if !self.eat_word("using") {
            return Ok(None);
        }
        // `index` is both a path word and the keyword that introduces a named
        // index, so the two forms are told apart by what follows: a name means
        // `USING INDEX by_email`, and anything else means the path word. One
        // token of lookahead, and no spelling has to be given up — `USING index`
        // asks whether *an* index answered, `USING INDEX by_email` asks which.
        if self.peek_keyword() == Some(Keyword::Index)
            && matches!(self.peek_ahead(1), Some(Token::Ident(_)))
            && !self.opens_the_next_clause()
        {
            self.advance();
            return Ok(Some(Using::Index(self.name()?)));
        }
        // `word_or_name`, because several path words are also keywords —
        // `index`, `join`, `record` — and a clause that accepted only the ones
        // that happen not to be would be a vocabulary decided by the lexer.
        Ok(Some(Using::Path(self.word_or_name()?)))
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

    /// `TIMEOUT 200ms`, when it is there.
    ///
    /// The duration is a literal rather than an expression, and a parameter is
    /// not accepted in its place. A ceiling that a bound value could set is a
    /// ceiling a caller could raise, and the statement is where this one is meant
    /// to be readable — an operator reading a slow query wants the budget in
    /// front of them, not in a bindings map somewhere else.
    pub(super) fn timeout(&mut self) -> Result<Option<Timeout>> {
        if !self.eat_word("timeout") {
            return Ok(None);
        }
        let expected = "a duration, like `200ms` or `5s`";
        let Some(Token::Duration(after)) = self.peek() else {
            return Err(self.error_here(expected));
        };
        let after = *after;
        let at = self.span_here();
        self.advance();
        // A ceiling of zero or less is refused here rather than at the read.
        // Neither names a budget a statement could satisfy, so the clause could
        // only ever refuse — and a clause that can only refuse is a mistake in
        // the statement, which is a thing to say when the statement is read.
        if after.seconds() < 0 || (after.seconds() == 0 && after.nanos() == 0) {
            return Err(Error::EmptyTimeout {
                written: after.to_literal(),
                span: at,
            });
        }
        Ok(Some(Timeout { after, span: at }))
    }

    /// The bound a conditional delete must carry: `LIMIT 100` or `LIMIT ALL`.
    ///
    /// Required, unlike every other `LIMIT` in this grammar. A read that omits
    /// one answers with more rows than the caller expected; a delete that omits
    /// one removes a table. `LIMIT ALL` is the way to say the second on purpose,
    /// and it costs one word — which is the entire mechanism.
    pub(super) fn delete_bound(&mut self) -> Result<DeleteBound> {
        let expected =
            "`LIMIT n` or `LIMIT ALL` — a conditional delete states how much it may remove";
        if !self.eat_word("limit") {
            return Err(self.error_here(expected));
        }
        if self.eat_word("all") {
            return Ok(DeleteBound::All);
        }
        let Some(Token::Number(Number::Integer(count))) = self.peek() else {
            return Err(self.error_here(expected));
        };
        let count = u64::try_from(*count).map_err(|_| self.error_here(expected))?;
        self.advance();
        Ok(DeleteBound::AtMost(count))
    }
}

/// What a cursor may be written beside, and which table its anchor may name.
///
/// Both refusals are properties of the statement, so neither waits for a read.
///
/// **A `START` beside an `AFTER`** is refused because the two are answers to the
/// same question — where does this page begin — and applying both means one of
/// them silently loses: the offset would count from the cursor's own position
/// and skip a page nobody asked to skip.
///
/// **An anchor from another table** is refused because a record identity carries
/// no table once it is compared. `orders:5` and `users:5` compare identically,
/// so a cursor pasted from the wrong page would page a real table by a real
/// identity and answer with records — the wrong ones, quietly. The check is
/// possible only where the source names one table; a join, a walk and a
/// materialised source each reach records from more than one place, and there is
/// no name there to disagree with.
///
/// **A clause that changes what a row is** is refused beside it for a third
/// reason: an anchor is a record, and `GROUP BY` answers with groups, `FETCH`
/// answers with records whose references have been opened, and `SPLIT ON`
/// answers with a row per element. In each of those the thing the cursor is
/// compared against is not the thing the anchor is, so the comparison would be
/// between two different kinds of row and the page would be decided by whichever
/// of them the sort key happened to reach.
pub(super) fn check_cursor(
    from: &Source,
    after: Option<&RecordTarget>,
    start: Option<u64>,
    reshaping: [(&'static str, bool); 3],
) -> Result<()> {
    let Some(anchor) = after else {
        return Ok(());
    };
    if start.is_some() {
        return Err(Error::CursorBesideAnOffset { span: anchor.span });
    }
    for (clause, written) in reshaping {
        if written {
            return Err(Error::CursorBesideAReshaping {
                clause,
                span: anchor.span,
            });
        }
    }
    let named = match from {
        Source::Table(table) | Source::Where { table, .. } => &table.name,
        Source::Record(target) => &target.table.name,
        Source::Node | Source::Traverse { .. } | Source::Join { .. } | Source::Subquery { .. } => {
            return Ok(());
        }
    };
    if !anchor.table.name.text.eq_ignore_ascii_case(&named.text) {
        return Err(Error::AnchorFromAnotherTable {
            anchor: anchor.table.name.text.clone(),
            table: named.text.clone(),
            span: anchor.span,
        });
    }
    Ok(())
}

/// A grouped read may project only its keys and its folds.
///
/// `SELECT name, count(*) AS n … GROUP BY city` is refused, because `name` has
/// as many values as the group has records and picking one silently is how a
/// wrong number reaches a report. It is a property of the statement, so it is
/// refused when the statement is read.
pub(super) fn check_grouping(projection: &Projection, group: &[Expr]) -> Result<()> {
    let Projection::Values { values, .. } = projection else {
        // `SELECT *` over a group would answer with whichever record came last.
        if group.is_empty() {
            return Ok(());
        }
        return Err(Error::UngroupedProjection {
            name: "*".to_owned(),
            span: Span::new(0, 0),
        });
    };
    let folds = values.iter().any(|value| holds_a_fold(&value.value));
    if !folds && group.is_empty() {
        return Ok(());
    }
    for value in values {
        if !grouped_by(&value.value, group) {
            return Err(Error::UngroupedProjection {
                name: value.name.text.clone(),
                span: value.value.span,
            });
        }
        nested_fold(&value.value)?;
    }
    Ok(())
}

/// Whether this expression has one value per group.
///
/// Recursive, because a projection may now be *built from* folds and keys rather
/// than being one: `mean(age) * 2` is admissible and `name` is not, and the
/// difference is a property of every part rather than of the whole.
///
/// - A **fold** has one value per group by definition, and what is inside it is
///   per-record and is not this rule's business.
/// - An expression with the **shape of a group key** has one value per group,
///   because that is what grouping by it means. Compared by shape rather than by
///   `==`, since the same expression written twice sits at two spans and would
///   otherwise never match itself.
/// - A **literal** is one value everywhere.
/// - Anything built out of those is one value per group.
///
/// What is left is a path, a parameter or a read that reaches into the record,
/// and each of those has as many values as the group has records — which is how
/// a wrong number reaches a report.
fn grouped_by(expr: &Expr, group: &[Expr]) -> bool {
    if group.iter().any(|key| key.same_shape(expr)) {
        return true;
    }
    match &expr.kind {
        ExprKind::Fold { .. } | ExprKind::Literal(_) => true,
        ExprKind::Not(inner) | ExprKind::Negate(inner) => grouped_by(inner, group),
        // Every arm has to be grouped, not just the one that will run: which
        // one runs is a property of the data, and whether a projection is legal
        // is a property of the statement.
        ExprKind::If {
            condition,
            then,
            otherwise,
        } => {
            grouped_by(condition, group)
                && grouped_by(then, group)
                && otherwise
                    .as_deref()
                    .is_none_or(|otherwise| grouped_by(otherwise, group))
        }
        ExprKind::Coalesce(left, right) => grouped_by(left, group) && grouped_by(right, group),
        ExprKind::And(left, right)
        | ExprKind::Or(left, right)
        | ExprKind::Arithmetic { left, right, .. }
        | ExprKind::Binary { left, right, .. } => {
            grouped_by(left, group) && grouped_by(right, group)
        }
        ExprKind::Call { arguments, .. } => {
            arguments.iter().all(|argument| grouped_by(argument, group))
        }
        ExprKind::Array(items) | ExprKind::Set(items) => {
            items.iter().all(|item| grouped_by(item, group))
        }
        ExprKind::Object(fields) => fields.iter().all(|field| grouped_by(&field.value, group)),
        // A path, a parameter, a table, a record, a range, a read: none of them
        // is one value per group unless it *is* a key, which was asked above.
        _ => false,
    }
}

/// Whether this expression holds a fold anywhere inside it.
fn holds_a_fold(expr: &Expr) -> bool {
    if matches!(expr.kind, ExprKind::Fold { .. }) {
        return true;
    }
    children(expr).into_iter().any(holds_a_fold)
}

/// A fold inside a fold is refused, and refused where the statement is read.
///
/// `mean(sum(price))` has no meaning at one grouping level: the inner fold has
/// already collapsed the records the outer one would fold over, so what is left
/// to average is a single number. It is a property of the statement, so nothing
/// has to run for it to be wrong.
fn nested_fold(expr: &Expr) -> Result<()> {
    if let ExprKind::Fold {
        over: Some(over),
        span,
        ..
    } = &expr.kind
        && holds_a_fold(over)
    {
        return Err(Error::FoldInsideAFold { span: *span });
    }
    for child in children(expr) {
        nested_fold(child)?;
    }
    Ok(())
}

/// The expressions one expression is built out of.
fn children(expr: &Expr) -> Vec<&Expr> {
    match &expr.kind {
        ExprKind::Fold { over, .. } => over.as_deref().into_iter().collect(),
        ExprKind::Not(inner) | ExprKind::Negate(inner) => vec![inner],
        ExprKind::If {
            condition,
            then,
            otherwise,
        } => {
            let mut parts = vec![&**condition, &**then];
            parts.extend(otherwise.as_deref());
            parts
        }
        ExprKind::Coalesce(left, right) => vec![left, right],
        ExprKind::And(left, right)
        | ExprKind::Or(left, right)
        | ExprKind::Arithmetic { left, right, .. }
        | ExprKind::Binary { left, right, .. } => vec![left, right],
        ExprKind::Call { arguments, .. } => arguments.iter().collect(),
        ExprKind::Array(items) | ExprKind::Set(items) => items.iter().collect(),
        ExprKind::Object(fields) => fields.iter().map(|field| &field.value).collect(),
        ExprKind::Range(range) => vec![&range.start, &range.end],
        _ => Vec::new(),
    }
}

/// A fold stands in a projection and nowhere else.
///
/// A filter sees one record at a time, so a fold in a `WHERE` is asking a
/// question the filter cannot be handed the records to answer — and what it
/// *means* is a filter over groups, which is `HAVING`: a second filter position
/// with its own scoping rule, and its own row in the specification's list of
/// absences. The refusal says which of the two it is, because "unexpected token"
/// would send the author looking for a typo.
///
/// An `ORDER BY` and a `GROUP BY` key are refused for the same reason: both are
/// evaluated per record, before there is a group to fold over.
pub(super) fn check_fold_positions(
    from: &Source,
    group: &[Expr],
    order: &[Ordering],
) -> Result<()> {
    match from {
        Source::Where { condition, .. } => no_fold(condition)?,
        Source::Join {
            condition: Some(condition),
            ..
        }
        | Source::Subquery {
            condition: Some(condition),
            ..
        } => no_fold(condition)?,
        // The inner read was checked as it was parsed, so there is nothing left
        // to say about it here.
        Source::Node
        | Source::Record(_)
        | Source::Table(_)
        | Source::Traverse { .. }
        | Source::Join { .. }
        | Source::Subquery { .. } => {}
    }
    for key in group {
        no_fold(key)?;
    }
    for ordering in order {
        no_fold(&ordering.key)?;
    }
    Ok(())
}

/// Refuse a fold anywhere in this expression.
pub(super) fn no_fold(expr: &Expr) -> Result<()> {
    if let ExprKind::Fold { span, .. } = &expr.kind {
        return Err(Error::FoldInAFilter { span: *span });
    }
    for child in children(expr) {
        no_fold(child)?;
    }
    Ok(())
}

/// A route reaching several values stands as the left operand of a comparison,
/// and nowhere else yet.
///
/// The right operand is excluded too: `'urgent' = tags[*]` would be the same
/// question written backwards, and giving it a second spelling before the first
/// one has a projection and an index is how a language grows two ways to ask
/// one thing.
pub(super) fn check_several(expr: &Expr) -> Result<()> {
    if let ExprKind::Binary { left, right, .. } = &expr.kind {
        // The one admitted position. What is under it still has to be checked —
        // `a[*].b[*]` is two relations composed, and composing them is its own
        // question.
        if let ExprKind::Path(field) = &left.kind
            && field.path.is_several()
        {
            return check_several(right);
        }
    }
    no_several(expr)
}

/// A route reaching several values stands as the **whole** projected value, and
/// nowhere inside a larger one.
///
/// A projection collects, so `tags[*] AS all_tags` answers with every value the
/// route reaches. `array::len(tags[*])` is refused because it has two defensible
/// answers — the function over the collected values, or the function applied to
/// each of them — and a language that picks one silently teaches the other by
/// surprise.
pub(super) fn check_projected(expr: &Expr) -> Result<()> {
    if let ExprKind::Path(field) = &expr.kind
        && field.path.is_several()
    {
        return Ok(());
    }
    no_several(expr)
}

/// Refuse a route reaching several values anywhere in this expression.
pub(super) fn no_several(expr: &Expr) -> Result<()> {
    if let ExprKind::Path(field) = &expr.kind
        && field.path.is_several()
    {
        return Err(Error::SeveralOutsideAComparison { span: field.span });
    }
    for child in children(expr) {
        // A comparison nested inside something else — `NOT tags[*] = 'x'`, or
        // one side of an `AND` — is still a comparison, so it keeps its rule.
        check_several(child)?;
    }
    Ok(())
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
