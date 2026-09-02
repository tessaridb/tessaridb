//! The abstract syntax, written back out as TessariQL.
//!
//! This is the other half of [`crate::parse`], and it lives beside it for one
//! reason: the failure worth preventing is the two directions **drifting** — a
//! renderer that emits something the parser no longer accepts. In one crate that
//! is a compile-and-test question; in two it becomes a version question, which
//! is the kind nobody notices until a caller's query is refused.
//!
//! # A statement nobody renders is a compile error
//!
//! The match over [`StatementKind`] is exhaustive and carries **no wildcard**.
//! Adding a statement to the grammar therefore fails to compile here until
//! somebody decides how it is written, which is the same guard the classifiers
//! over this enum already use. `SELECT` is the only form this milestone renders,
//! and that is a staging order rather than a permanent shape: every other form
//! names itself in [`crate::Error::Unrenderable`] instead of vanishing into a
//! catch-all.
//!
//! # Values do not appear here
//!
//! A caller's value reaches a script as a parameter and never as text, so the
//! renderer has nothing to escape and no quoting rules to keep in step with the
//! lexer's. That is not an omission this module works around — it is the
//! property that makes a built query safe, and it is asserted by test rather
//! than claimed.

use crate::ast::{
    Approximation, Expr, ExprKind, FieldPath, Ordering, Projected, Projection, Script, Select,
    Source, Statement, StatementKind, TableRef, Using,
};
use crate::error::{Error, Result};
use crate::token::Span;

/// A script, written back out as TessariQL.
///
/// The text is a **normal form** rather than a reproduction: clause order is the
/// grammar's, every projection carries its `AS`, and every compound condition is
/// parenthesised. Two trees that say the same thing render identically, which is
/// what lets the text itself be compared.
///
/// # Errors
///
/// [`Error::Unrenderable`] when the script holds a statement this milestone does
/// not write back out. The failure names the statement rather than the token.
pub fn render(script: &Script) -> Result<String> {
    let mut out = String::new();
    for (at, statement) in script.statements.iter().enumerate() {
        if at > 0 {
            out.push('\n');
        }
        write_statement(&mut out, statement)?;
        out.push(';');
    }
    Ok(out)
}

/// A statement's own text, without its terminator.
fn write_statement(out: &mut String, statement: &Statement) -> Result<()> {
    let span = statement.span;
    // Exhaustive and wildcard-free on purpose: see the module documentation.
    // Each unrendered form is listed by name so that the failure says which
    // statement it met, which a merged arm could not.
    match &statement.kind {
        StatementKind::Select(select) => write_select(out, select),
        StatementKind::Use { .. } => Err(unrenderable("USE", span)),
        StatementKind::DefineNamespace { .. } => Err(unrenderable("DEFINE NAMESPACE", span)),
        StatementKind::DefineDatabase { .. } => Err(unrenderable("DEFINE DATABASE", span)),
        StatementKind::DefineTable { .. } => Err(unrenderable("DEFINE TABLE", span)),
        StatementKind::DefineSpace { .. } => Err(unrenderable("DEFINE SPACE", span)),
        StatementKind::DefineBucket { .. } => Err(unrenderable("DEFINE BUCKET", span)),
        StatementKind::DefineCollection { .. } => Err(unrenderable("DEFINE COLLECTION", span)),
        StatementKind::DefineVector { .. } => Err(unrenderable("DEFINE VECTOR", span)),
        StatementKind::DropVector { .. } => Err(unrenderable("DROP VECTOR", span)),
        StatementKind::DefineGeo { .. } => Err(unrenderable("DEFINE GEO", span)),
        StatementKind::DropGeo { .. } => Err(unrenderable("DROP GEO", span)),
        StatementKind::DefineIndex { .. } => Err(unrenderable("DEFINE INDEX", span)),
        StatementKind::DefineField { .. } => Err(unrenderable("DEFINE FIELD", span)),
        StatementKind::DefineAnalyzer { .. } => Err(unrenderable("DEFINE ANALYZER", span)),
        StatementKind::DefineUser { .. } => Err(unrenderable("DEFINE USER", span)),
        StatementKind::AlterUser { .. } => Err(unrenderable("ALTER USER", span)),
        StatementKind::DefineNode { .. } => Err(unrenderable("DEFINE NODE", span)),
        StatementKind::DefineReplica { .. } => Err(unrenderable("DEFINE REPLICA", span)),
        StatementKind::DefineConsumer { .. } => Err(unrenderable("DEFINE CONSUMER", span)),
        StatementKind::DropConsumer { .. } => Err(unrenderable("DROP CONSUMER", span)),
        StatementKind::Explain(_) => Err(unrenderable("EXPLAIN", span)),
        StatementKind::Info { .. } => Err(unrenderable("INFO", span)),
        StatementKind::Backup { .. } => Err(unrenderable("BACKUP", span)),
        StatementKind::DropUser { .. } => Err(unrenderable("DROP USER", span)),
        StatementKind::DropField { .. } => Err(unrenderable("DROP FIELD", span)),
        StatementKind::DropTable { .. } => Err(unrenderable("DROP TABLE", span)),
        StatementKind::DropIndex { .. } => Err(unrenderable("DROP INDEX", span)),
        StatementKind::DropAnalyzer { .. } => Err(unrenderable("DROP ANALYZER", span)),
        StatementKind::DropReplica { .. } => Err(unrenderable("DROP REPLICA", span)),
        StatementKind::DropDatabase { .. } => Err(unrenderable("DROP DATABASE", span)),
        StatementKind::DropNamespace { .. } => Err(unrenderable("DROP NAMESPACE", span)),
        StatementKind::DefineGraph { .. } => Err(unrenderable("DEFINE GRAPH", span)),
        StatementKind::DropGraph { .. } => Err(unrenderable("DROP GRAPH", span)),
        StatementKind::DefineEdge { .. } => Err(unrenderable("DEFINE EDGE", span)),
        StatementKind::DropEdge { .. } => Err(unrenderable("DROP EDGE", span)),
        StatementKind::AlterTable { .. } => Err(unrenderable("ALTER TABLE", span)),
        StatementKind::AlterField { .. } => Err(unrenderable("ALTER TABLE ALTER FIELD", span)),
        StatementKind::RebuildIndex { .. } => Err(unrenderable("REBUILD INDEX", span)),
        StatementKind::Grant { .. } => Err(unrenderable("GRANT", span)),
        StatementKind::Revoke { .. } => Err(unrenderable("REVOKE", span)),
        StatementKind::GrantAuthority { .. } => Err(unrenderable("GRANT", span)),
        StatementKind::RevokeAuthority { .. } => Err(unrenderable("REVOKE", span)),
        StatementKind::Relate { .. } => Err(unrenderable("RELATE", span)),
        StatementKind::Create { .. } => Err(unrenderable("CREATE", span)),
        StatementKind::Insert { .. } => Err(unrenderable("INSERT", span)),
        StatementKind::Update { .. } => Err(unrenderable("UPDATE", span)),
        StatementKind::Upsert { .. } => Err(unrenderable("UPSERT", span)),
        StatementKind::Throw { .. } => Err(unrenderable("THROW", span)),
        StatementKind::Delete { .. } => Err(unrenderable("DELETE", span)),
        StatementKind::DeleteEdge { .. } => Err(unrenderable("DELETE of an edge", span)),
        StatementKind::DeleteWhere { .. } => Err(unrenderable("DELETE FROM", span)),
        StatementKind::Get { .. } => Err(unrenderable("GET", span)),
        StatementKind::Set { .. } => Err(unrenderable("SET", span)),
        StatementKind::Del { .. } => Err(unrenderable("DEL", span)),
        StatementKind::Put { .. } => Err(unrenderable("PUT", span)),
        StatementKind::Read { .. } => Err(unrenderable("READ", span)),
        StatementKind::Keys { .. } => Err(unrenderable("KEYS", span)),
        StatementKind::Let { .. } => Err(unrenderable("LET", span)),
        StatementKind::Return { .. } => Err(unrenderable("RETURN", span)),
        StatementKind::Begin => Err(unrenderable("BEGIN", span)),
        StatementKind::Commit => Err(unrenderable("COMMIT", span)),
        StatementKind::Cancel => Err(unrenderable("CANCEL", span)),
        StatementKind::Verify => Err(unrenderable("VERIFY", span)),
    }
}

/// The failure a form outside this milestone's coverage raises.
const fn unrenderable(statement: &'static str, span: Span) -> Error {
    Error::Unrenderable { statement, span }
}

/// `SELECT … FROM … [FETCH …] [GROUP BY …] [ORDER BY …] [START n] [LIMIT n]
///  [APPROXIMATE] [USING …]`
///
/// Clause order is the parser's, which is also application order — the grammar
/// keeps the two the same on purpose, so there is nothing to choose here.
fn write_select(out: &mut String, select: &Select) -> Result<()> {
    out.push_str("SELECT ");
    write_projection(out, &select.projection)?;
    if let Some((first, rest)) = select.omit.split_first() {
        out.push_str(" OMIT ");
        write_path(out, first);
        for route in rest {
            out.push_str(", ");
            write_path(out, route);
        }
    }
    out.push_str(" FROM ");
    if select.only.is_some() {
        out.push_str("ONLY ");
    }
    write_source(out, &select.from, select.span)?;

    if let Some((first, rest)) = select.fetch.split_first() {
        out.push_str(" FETCH ");
        write_path(out, first);
        for route in rest {
            out.push_str(", ");
            write_path(out, route);
        }
    }
    if let Some(route) = &select.split {
        out.push_str(" SPLIT ON ");
        write_path(out, route);
    }
    if let Some((first, rest)) = select.group.split_first() {
        out.push_str(" GROUP BY ");
        write_expr(out, first)?;
        for key in rest {
            out.push_str(", ");
            write_expr(out, key)?;
        }
    }
    if let Some((first, rest)) = select.order.split_first() {
        out.push_str(" ORDER BY ");
        write_ordering(out, first)?;
        for ordering in rest {
            out.push_str(", ");
            write_ordering(out, ordering)?;
        }
    }
    // A cursor names a record identity, and an identity is the one thing this
    // renderer has never learned to write — which is also why a read of one
    // record is unrenderable above. The builder cannot produce either, so this
    // arm is unreachable from the only caller; it is here so that it stays that
    // way rather than becoming a clause quietly dropped from rendered text.
    if select.after.is_some() {
        return Err(unrenderable("a cursor", select.span));
    }
    if let Some(start) = select.start {
        out.push_str(" START ");
        out.push_str(&start.to_string());
    }
    if let Some(limit) = select.limit {
        out.push_str(" LIMIT ");
        out.push_str(&limit.to_string());
    }
    match select.approximate {
        None => {}
        Some(Approximation::Default) => out.push_str(" APPROXIMATE"),
        Some(Approximation::Effort(candidates)) => {
            out.push_str(" APPROXIMATE EFFORT ");
            out.push_str(&candidates.to_string());
        }
    }
    match &select.using {
        Some(Using::Path(name)) => {
            out.push_str(" USING ");
            out.push_str(&name.text);
        }
        Some(Using::Index(name)) => {
            out.push_str(" USING INDEX ");
            out.push_str(&name.text);
        }
        None => {}
    }
    Ok(())
}

/// One sort key. `ASC` is left unwritten, because it is the default and the
/// parser accepts it only as a courtesy.
fn write_ordering(out: &mut String, ordering: &Ordering) -> Result<()> {
    write_expr(out, &ordering.key)?;
    if ordering.descending {
        out.push_str(" DESC");
    }
    Ok(())
}

/// `*`, or the projected values in the order they were written.
///
/// Every value carries an explicit `AS`, including one whose name the parser
/// would have inferred. Rendering the inferred case bare would be shorter and
/// would put the renderer in the business of re-deriving the default-name rule
/// — a second copy of `projected()`'s logic, drifting from the first.
fn write_projection(out: &mut String, projection: &Projection) -> Result<()> {
    match projection {
        Projection::All => {
            out.push('*');
            Ok(())
        }
        Projection::Values { everything, values } => {
            // The star first, always, whatever position it was written in: the
            // answer is ordered by name, so where it stood cannot be observed,
            // and one canonical place is what keeps a rendered statement stable.
            let mut written = everything.is_some();
            if written {
                out.push('*');
            }
            for value in values {
                if written {
                    out.push_str(", ");
                }
                write_projected(out, value)?;
                written = true;
            }
            Ok(())
        }
    }
}

/// One projected value and the name it answers under.
fn write_projected(out: &mut String, projected: &Projected) -> Result<()> {
    write_expr(out, &projected.value)?;
    out.push_str(" AS ");
    out.push_str(&projected.name.text);
    Ok(())
}

/// What the `FROM` names.
///
/// Two of the six access paths are written here. The rest name themselves, for
/// the reason the statement match gives. A source carries no span of its own, so
/// a failure points at the read it belongs to.
fn write_source(out: &mut String, source: &Source, span: Span) -> Result<()> {
    let unwritten = |what: &'static str| Err(unrenderable(what, span));
    match source {
        Source::Table(table) => {
            write_table(out, table);
            Ok(())
        }
        Source::Where { table, condition } => {
            write_table(out, table);
            out.push_str(" WHERE ");
            write_expr(out, condition)
        }
        Source::Node => unwritten("a read of $node"),
        Source::Record(_) => unwritten("a read of one record"),
        Source::Traverse { .. } => unwritten("a traversal"),
        Source::Join { .. } => unwritten("a join"),
        Source::Subquery { .. } => unwritten("a materialised read"),
    }
}

/// `users` or `orders.users`.
fn write_table(out: &mut String, table: &TableRef) {
    if let Some(database) = &table.database {
        out.push_str(&database.text);
        out.push('.');
    }
    out.push_str(&table.name.text);
}

/// A route into a record: `name`, `address.city`, `tags[0]`, `tags[*]`.
fn write_path(out: &mut String, path: &FieldPath) {
    out.push_str(&path.path.to_string());
}

/// One expression.
///
/// Every compound form is parenthesised. That costs a few characters and buys
/// the property this module exists for: precedence never has to be reasoned
/// about, so the text cannot be read back as a different tree. Parentheses are
/// transparent to the syntax — the parser widens the span and keeps the kind —
/// so a parenthesised render still compares equal to what produced it.
fn write_expr(out: &mut String, expr: &Expr) -> Result<()> {
    let span = expr.span;
    let unwritten = |what: &'static str| Err(unrenderable(what, span));
    match &expr.kind {
        ExprKind::Path(path) => {
            write_path(out, path);
            Ok(())
        }
        ExprKind::Parameter(name) => {
            out.push('$');
            out.push_str(name);
            Ok(())
        }
        ExprKind::Binary { op, left, right } => write_joined(out, left, op.spelling(), right),
        ExprKind::Arithmetic { op, left, right } => write_joined(out, left, op.spelling(), right),
        ExprKind::And(left, right) => write_joined(out, left, "AND", right),
        ExprKind::Or(left, right) => write_joined(out, left, "OR", right),
        ExprKind::Not(inner) => {
            out.push_str("(NOT ");
            write_expr(out, inner)?;
            out.push(')');
            Ok(())
        }
        ExprKind::Negate(_) => unwritten("a negation"),
        ExprKind::Literal(_) => unwritten("a literal value"),
        ExprKind::Fold { .. } => unwritten("a fold"),
        ExprKind::Call { .. } => unwritten("a function call"),
        ExprKind::Table(_) => unwritten("a table in a value position"),
        ExprKind::Record(_) => unwritten("a record in a value position"),
        ExprKind::Array(_) => unwritten("an array"),
        ExprKind::Set(_) => unwritten("a set"),
        ExprKind::Object(_) => unwritten("an object"),
        ExprKind::Range(_) => unwritten("a range"),
        ExprKind::If { .. } => unwritten("a conditional"),
        ExprKind::Coalesce(..) => unwritten("a coalesce"),
        ExprKind::Get(_) => unwritten("an embedded GET"),
        ExprKind::Select(_) => unwritten("an embedded read"),
    }
}

/// `(<left> <word> <right>)` — the shape both connectives share.
fn write_joined(out: &mut String, left: &Expr, word: &str, right: &Expr) -> Result<()> {
    out.push('(');
    write_expr(out, left)?;
    out.push(' ');
    out.push_str(word);
    out.push(' ');
    write_expr(out, right)?;
    out.push(')');
    Ok(())
}
