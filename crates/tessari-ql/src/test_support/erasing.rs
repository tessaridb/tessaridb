use super::*;

/// One read, and everything under it.
pub(super) fn erase_select(select: &mut Select) {
    select.span = CANONICAL;
    if let Some(at) = &mut select.only {
        *at = CANONICAL;
    }
    match &mut select.projection {
        Projection::All => {}
        Projection::Values { everything, values } => {
            if let Some(at) = everything {
                *at = CANONICAL;
            }
            for projected in values {
                erase_expr(&mut projected.value);
                erase_name(&mut projected.name);
            }
        }
    }
    erase_source(&mut select.from);
    if let Some(route) = &mut select.split {
        erase_path(route);
    }
    for route in &mut select.omit {
        erase_path(route);
    }
    for route in &mut select.fetch {
        erase_path(route);
    }
    for key in &mut select.group {
        erase_expr(key);
    }
    for ordering in &mut select.order {
        erase_expr(&mut ordering.key);
    }
    if let Some(fusion) = &mut select.fusion {
        fusion.span = CANONICAL;
    }
}

/// What a `FROM` names.
pub(super) fn erase_source(source: &mut Source) {
    match source {
        Source::Node => {}
        Source::Record(record) => erase_record(record),
        Source::Table(table) => erase_table(table),
        Source::Range { table, span, .. } => {
            erase_table(table);
            *span = CANONICAL;
        }
        Source::Traverse { from, hops, .. } => {
            erase_record(from);
            for hop in hops {
                erase_table(&mut hop.edges);
                if let Some(target) = &mut hop.target {
                    erase_table(target);
                }
            }
        }
        Source::Where { table, condition } => {
            erase_table(table);
            erase_expr(condition);
        }
        Source::Join {
            left,
            right,
            left_key,
            right_key,
            asof: _,
            condition,
        } => {
            erase_join_side(left);
            erase_join_side(right);
            erase_path(left_key);
            erase_path(right_key);
            if let Some(condition) = condition {
                erase_expr(condition);
            }
        }
        Source::Subquery { read, condition } => {
            erase_select(read);
            if let Some(condition) = condition {
                erase_expr(condition);
            }
        }
        Source::Search {
            name,
            ask,
            condition,
        } => {
            erase_name(name);
            match ask {
                crate::ast::SearchAsk::Matches { query, .. } => erase_expr(query),
                crate::ast::SearchAsk::Complete { beginning } => erase_expr(beginning),
            }
            if let Some(condition) = condition {
                erase_expr(condition);
            }
        }
    }
}

/// One side of a join, whichever of the two it is.
pub(super) fn erase_join_side(side: &mut JoinSide) {
    match side {
        JoinSide::Table { table, alias } => {
            erase_table(table);
            if let Some(alias) = alias {
                erase_name(alias);
            }
        }
        JoinSide::Read { read, alias } => {
            erase_select(read);
            erase_name(alias);
        }
    }
}

/// One expression, and everything under it.
pub(super) fn erase_expr(expr: &mut Expr) {
    expr.span = CANONICAL;
    match &mut expr.kind {
        // A value holds no spans, and a parameter is a name without one.
        ExprKind::Literal(_) | ExprKind::Parameter(_) => {}
        ExprKind::Path(path) => erase_path(path),
        ExprKind::Not(inner) | ExprKind::Negate(inner) => erase_expr(inner),
        ExprKind::Route { value, .. } => erase_expr(value),
        ExprKind::If {
            condition,
            then,
            otherwise,
        } => {
            erase_expr(condition);
            erase_expr(then);
            if let Some(otherwise) = otherwise {
                erase_expr(otherwise);
            }
        }
        ExprKind::Coalesce(left, right) => {
            erase_expr(left);
            erase_expr(right);
        }
        // The first of the two sites a search for `pub span: Span` cannot see.
        ExprKind::Fold { over, at, span, .. } => {
            *span = CANONICAL;
            if let Some(over) = over {
                erase_expr(over);
            }
            if let Some(at) = at {
                erase_expr(at);
            }
        }
        // The second.
        ExprKind::Call {
            arguments, span, ..
        } => {
            *span = CANONICAL;
            for argument in arguments {
                erase_expr(argument);
            }
        }
        ExprKind::And(left, right) | ExprKind::Or(left, right) => {
            erase_expr(left);
            erase_expr(right);
        }
        ExprKind::Arithmetic { left, right, .. } | ExprKind::Binary { left, right, .. } => {
            erase_expr(left);
            erase_expr(right);
        }
        ExprKind::Table(table) => erase_table(table),
        ExprKind::Record(record) | ExprKind::Get(record) | ExprKind::Ttl(record) => {
            erase_record(record);
        }
        ExprKind::Array(items) | ExprKind::Set(items) => {
            for item in items {
                erase_expr(item);
            }
        }
        ExprKind::Object(fields) => {
            for field in fields {
                erase_name(&mut field.name);
                erase_expr(&mut field.value);
            }
        }
        ExprKind::Range(range) => {
            erase_expr(&mut range.start);
            erase_expr(&mut range.end);
        }
        ExprKind::Select(select) => erase_select(select),
    }
}

/// A reach, in whichever of its three spellings the statement used.
pub(super) fn erase_reach(reach: &mut ReachRef) {
    match reach {
        ReachRef::Store => {}
        ReachRef::Namespace(name) => erase_name(name),
        ReachRef::Database(table) => erase_table(table),
        ReachRef::Shard {
            namespace,
            database,
            table,
            ..
        } => {
            erase_name(namespace);
            erase_name(database);
            erase_name(table);
        }
    }
}
