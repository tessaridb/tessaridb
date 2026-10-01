//! Writing a read, its projection, source and expressions back as text.

use super::{unrenderable, write_joined, write_ordering, write_path, write_projected, write_table};
use crate::ast::{
    Approximation, Expr, ExprKind, Fusion, Ordering, Projection, Select, Source, Using,
};
use crate::error::Result;
use crate::token::Span;
use tessari_types::Number;

/// `SELECT … FROM … [FETCH …] [GROUP BY …] [ORDER BY …] [START n] [LIMIT n]
///  [APPROXIMATE] [USING …]`
///
/// Clause order is the parser's, which is also application order — the grammar
/// keeps the two the same on purpose, so there is nothing to choose here.
pub(crate) fn write_select(out: &mut String, select: &Select) -> Result<()> {
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
    if let Some(fusion) = &select.fusion {
        write_fusion(out, &select.order, fusion)?;
    } else if let Some((first, rest)) = select.order.split_first() {
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

/// `ORDER BY FUSE (…) [DEPTH n]`. A weight of one is left unwritten, because it
/// is what a branch without `WEIGHT` parses to.
pub(crate) fn write_fusion(out: &mut String, branches: &[Ordering], fusion: &Fusion) -> Result<()> {
    out.push_str(" ORDER BY FUSE (");
    for (position, branch) in branches.iter().enumerate() {
        if position > 0 {
            out.push_str(", ");
        }
        write_ordering(out, branch)?;
        if let Some(weight) = fusion.weights.get(position)
            && *weight != Number::Integer(1)
        {
            out.push_str(" WEIGHT ");
            out.push_str(&weight.to_string());
        }
    }
    out.push(')');
    if let Some(depth) = fusion.depth {
        out.push_str(" DEPTH ");
        out.push_str(&depth.to_string());
    }
    Ok(())
}

/// `*`, or the projected values in the order they were written.
///
/// Every value carries an explicit `AS`, including one whose name the parser
/// would have inferred. Rendering the inferred case bare would be shorter and
/// would put the renderer in the business of re-deriving the default-name rule
/// — a second copy of `projected()`'s logic, drifting from the first.
pub(crate) fn write_projection(out: &mut String, projection: &Projection) -> Result<()> {
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

/// What the `FROM` names.
///
/// Two of the six access paths are written here. The rest name themselves, for
/// the reason the statement match gives. A source carries no span of its own, so
/// a failure points at the read it belongs to.
pub(crate) fn write_source(out: &mut String, source: &Source, span: Span) -> Result<()> {
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
        Source::Range { .. } => unwritten("a read of a span of identities"),
        Source::Traverse { .. } => unwritten("a traversal"),
        Source::Join { .. } => unwritten("a join"),
        Source::Subquery { .. } => unwritten("a materialised read"),
        Source::Search { .. } => unwritten("a read of a search"),
    }
}

/// One expression.
///
/// Every compound form is parenthesised. That costs a few characters and buys
/// the property this module exists for: precedence never has to be reasoned
/// about, so the text cannot be read back as a different tree. Parentheses are
/// transparent to the syntax — the parser widens the span and keeps the kind —
/// so a parenthesised render still compares equal to what produced it.
pub(crate) fn write_expr(out: &mut String, expr: &Expr) -> Result<()> {
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
        // A call is its spelling and its arguments, each written as any other
        // expression — so a literal argument is refused here like any literal.
        ExprKind::Call {
            function,
            arguments,
            ..
        } => {
            out.push_str(function.spelling());
            out.push('(');
            for (position, argument) in arguments.iter().enumerate() {
                if position > 0 {
                    out.push_str(", ");
                }
                write_expr(out, argument)?;
            }
            out.push(')');
            Ok(())
        }
        ExprKind::Table(_) => unwritten("a table in a value position"),
        ExprKind::Record(_) => unwritten("a record in a value position"),
        // Its items written like a call's arguments, so a literal item is
        // refused here like any literal and travels lifted (ADR-0102).
        ExprKind::Array(items) => {
            out.push('[');
            for (position, item) in items.iter().enumerate() {
                if position > 0 {
                    out.push_str(", ");
                }
                write_expr(out, item)?;
            }
            out.push(']');
            Ok(())
        }
        ExprKind::Set(_) => unwritten("a set"),
        ExprKind::Object(_) => unwritten("an object"),
        ExprKind::Range(_) => unwritten("a range"),
        ExprKind::If { .. } => unwritten("a conditional"),
        ExprKind::Coalesce(..) => unwritten("a coalesce"),
        ExprKind::Get(_) => unwritten("an embedded GET"),
        ExprKind::Ttl(_) => unwritten("an embedded TTL"),
        ExprKind::Select(_) => unwritten("an embedded read"),
    }
}
