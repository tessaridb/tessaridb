//! Turning a view's name into the read it stands for.
//!
//! # Where this runs, and why that is the whole design
//!
//! **Before the statement is authorized, not while it is executed.**
//! [`crate::session::Session::step`] authorizes a statement and then executes
//! it, and the grant check reads the tables a statement names off the parsed
//! tree (`reach::tables_named`). An expansion happening any later would be
//! invisible to that check: a caller granted `read` on the view and nothing else
//! would read the base table through it, and the statement would succeed. That
//! is a definer-rights view arrived at by accident, and it is the exact failure
//! this ordering removes.
//!
//! Running before the check also buys the property that makes it safe: **the
//! tree that is authorized is the tree that is executed.** There is no second
//! lookup that could disagree with the first, and no window in which a view's
//! body changes between the check and the run — a view is immutable while it
//! exists, since the catalog refuses a repeat definition of a name it already
//! holds, so changing one means dropping it first.
//!
//! # What a view becomes
//!
//! `FROM v` becomes a materialised source holding the view's read, and
//! `FROM v WHERE c` puts `c` on that source rather than inside the read. The
//! difference is not stylistic. A view may project names the base record does
//! not carry, and a condition merged into the view's own source would be tested
//! against the base record — so `DEFINE VIEW v AS SELECT name, city FROM users`
//! followed by `SELECT * FROM v WHERE salary > 100` would filter on a field the
//! view does not answer with, and answer a different question with nothing in an
//! error state.
//!
//! # What it costs, said here because it is easy to forget
//!
//! A materialised source is held whole before the outer statement asks anything
//! of it, so a bounded read over a view does **not** stop where the answer fills
//! the way the same read over a table does. A view is a name for a read, not a
//! faster way to run one. It is bounded rather than unbounded: a view naming no
//! `LIMIT` runs under the ceiling every held read runs under, and past it the
//! read is refused rather than truncated.
//!
//! # Names this does not touch
//!
//! A view named anywhere but a read source is left alone, and refuses at
//! [`crate::context`]'s table resolution instead. That is deliberate and it is
//! what makes this walk's coverage a usability question rather than a
//! correctness one: a position it does not reach refuses, and never quietly
//! reads a view's empty prefix and answers nothing.

use tessari_constants::MAX_VIEW_DEPTH;
use tessari_ql::{
    Expr, ExprKind, JoinSide, Projection, Select, Source, StatementKind, TableRef, parse_read,
};
use tessari_storage::{Catalog, Store, TableKind};

use crate::error::{Error, Result};
use crate::session::Session;

impl Session<'_> {
    /// The statement with every view in a read position replaced by its read.
    ///
    /// `None` when the statement named no view, which is every statement in a
    /// store that has none — the walk is over the tree that is already in hand
    /// and the catalog is only asked about a name a read actually mentions.
    ///
    /// # Errors
    ///
    /// Returns [`Error::ViewsTooDeep`] when a chain of views is longer than this
    /// build follows, [`Error::ViewUnreadable`] when a stored read no longer
    /// parses, and a substrate failure when the catalog cannot be read.
    pub(crate) fn expand_views(
        &self,
        store: &Store,
        kind: &StatementKind,
    ) -> Result<Option<StatementKind>> {
        // Only the two read statements. Everything else that can name a view
        // names it somewhere a view has no records to serve, and is refused when
        // the name is resolved.
        let read = match kind {
            StatementKind::Select(read) | StatementKind::Explain(read) => read,
            _ => return Ok(None),
        };
        let mut rewritten = read.clone();
        let mut chain = Vec::new();
        if !self.expand_read(store, &mut rewritten, &mut chain)? {
            return Ok(None);
        }
        Ok(Some(match kind {
            StatementKind::Select(_) => StatementKind::Select(rewritten),
            _ => StatementKind::Explain(rewritten),
        }))
    }

    /// Expand every view this read names, answering whether anything changed.
    fn expand_read(
        &self,
        store: &Store,
        read: &mut Select,
        chain: &mut Vec<String>,
    ) -> Result<bool> {
        if chain.len() > MAX_VIEW_DEPTH {
            return Err(Error::ViewsTooDeep {
                chain: chain.clone(),
                depth: MAX_VIEW_DEPTH,
                span: read.span,
            });
        }
        let mut changed = self.expand_source(store, read, chain)?;
        if let Projection::Values { values, .. } = &mut read.projection {
            for one in values {
                changed |= self.expand_expr(store, &mut one.value, chain)?;
            }
        }
        for key in &mut read.group {
            changed |= self.expand_expr(store, key, chain)?;
        }
        for ordering in &mut read.order {
            changed |= self.expand_expr(store, &mut ordering.key, chain)?;
        }
        Ok(changed)
    }

    /// Expand the read's source, which is the position a view is written in.
    fn expand_source(
        &self,
        store: &Store,
        read: &mut Select,
        chain: &mut Vec<String>,
    ) -> Result<bool> {
        // Taken out and put back, because the two view arms replace the source
        // with one built from it and a borrow of the old one cannot outlive that.
        let source = std::mem::replace(&mut read.from, Source::Node);
        let (source, changed) = match source {
            Source::Table(table) => match self.view_read(store, &table, chain)? {
                Some(inner) => (
                    Source::Subquery {
                        read: Box::new(inner),
                        condition: None,
                    },
                    true,
                ),
                None => (Source::Table(table), false),
            },
            Source::Where { table, condition } => match self.view_read(store, &table, chain)? {
                // The condition rides on the materialised source rather than
                // into the view's own read: it may ask about a name the view
                // produced, and it must never be tested against the base record.
                Some(inner) => (
                    Source::Subquery {
                        read: Box::new(inner),
                        condition: Some(condition),
                    },
                    true,
                ),
                None => {
                    let mut condition = condition;
                    let changed = self.expand_expr(store, &mut condition, chain)?;
                    (Source::Where { table, condition }, changed)
                }
            },
            Source::Subquery {
                mut read,
                mut condition,
            } => {
                let mut changed = self.expand_read(store, &mut read, chain)?;
                if let Some(condition) = &mut condition {
                    changed |= self.expand_expr(store, condition, chain)?;
                }
                (Source::Subquery { read, condition }, changed)
            }
            Source::Join {
                mut left,
                mut right,
                left_key,
                right_key,
                mut condition,
            } => {
                let mut changed = self.expand_join_side(store, &mut left, chain)?;
                changed |= self.expand_join_side(store, &mut right, chain)?;
                if let Some(condition) = &mut condition {
                    changed |= self.expand_expr(store, condition, chain)?;
                }
                (
                    Source::Join {
                        left,
                        right,
                        left_key,
                        right_key,
                        condition,
                    },
                    changed,
                )
            }
            // A record, a span and a walk are all **keyspace** addresses, and a
            // view has no keyspace. Left as written so that the refusal comes
            // from the resolution, in a message about what a view is rather than
            // about what this rewrite declined to do.
            source @ (Source::Node
            | Source::Record(_)
            | Source::Range { .. }
            | Source::Traverse { .. }) => (source, false),
        };
        read.from = source;
        Ok(changed)
    }

    /// Expand a join side, whose read half may name a view like any other.
    fn expand_join_side(
        &self,
        store: &Store,
        side: &mut JoinSide,
        chain: &mut Vec<String>,
    ) -> Result<bool> {
        match side {
            // A table side is left alone: a row files each side under a name,
            // and a materialised read has no name of its own, so turning one
            // into the other would take the row's key away. `FROM (SELECT * FROM
            // v LIMIT n) AS x JOIN …` is how a view joins, and it is written.
            JoinSide::Table { .. } => Ok(false),
            JoinSide::Read { read, .. } => self.expand_read(store, read, chain),
        }
    }

    /// Expand every read standing inside an expression.
    ///
    /// The same walk `reach::tables_named` makes over expressions, and for the
    /// same reason: an expression may hold a read, and a read may name a view.
    fn expand_expr(&self, store: &Store, expr: &mut Expr, chain: &mut Vec<String>) -> Result<bool> {
        match &mut expr.kind {
            ExprKind::Select(read) => self.expand_read(store, read, chain),
            ExprKind::Not(inner) | ExprKind::Negate(inner) => self.expand_expr(store, inner, chain),
            ExprKind::If {
                condition,
                then,
                otherwise,
            } => {
                let mut changed = self.expand_expr(store, condition, chain)?;
                changed |= self.expand_expr(store, then, chain)?;
                if let Some(otherwise) = otherwise {
                    changed |= self.expand_expr(store, otherwise, chain)?;
                }
                Ok(changed)
            }
            ExprKind::Coalesce(left, right)
            | ExprKind::And(left, right)
            | ExprKind::Or(left, right)
            | ExprKind::Arithmetic { left, right, .. }
            | ExprKind::Binary { left, right, .. } => {
                let mut changed = self.expand_expr(store, left, chain)?;
                changed |= self.expand_expr(store, right, chain)?;
                Ok(changed)
            }
            ExprKind::Fold { over, .. } => match over {
                Some(over) => self.expand_expr(store, over, chain),
                None => Ok(false),
            },
            ExprKind::Call { arguments, .. } => {
                let mut changed = false;
                for argument in arguments {
                    changed |= self.expand_expr(store, argument, chain)?;
                }
                Ok(changed)
            }
            ExprKind::Array(items) | ExprKind::Set(items) => {
                let mut changed = false;
                for item in items {
                    changed |= self.expand_expr(store, item, chain)?;
                }
                Ok(changed)
            }
            ExprKind::Object(fields) => {
                let mut changed = false;
                for field in fields {
                    changed |= self.expand_expr(store, &mut field.value, chain)?;
                }
                Ok(changed)
            }
            ExprKind::Range(range) => {
                let mut changed = self.expand_expr(store, &mut range.start, chain)?;
                changed |= self.expand_expr(store, &mut range.end, chain)?;
                Ok(changed)
            }
            // A literal, a parameter, a path, a bare table name and a point read
            // name no read. The two record forms are keyspace addresses, and a
            // view in one refuses at the resolution.
            ExprKind::Literal(_)
            | ExprKind::Parameter(_)
            | ExprKind::Path(_)
            | ExprKind::Table(_)
            | ExprKind::Record(_)
            | ExprKind::Get(_) => Ok(false),
        }
    }

    /// The read this name stands for, when the name is a view.
    ///
    /// Answers `None` for a table, for a name that resolves to nothing, and for
    /// a name whose tenancy cannot be worked out — none of which is this
    /// function's refusal to make. A name that does not resolve is refused by
    /// whatever resolves it, with a message about the table rather than about a
    /// view, which is the rule the grant check already states for itself.
    fn view_read(
        &self,
        store: &Store,
        table: &TableRef,
        chain: &mut Vec<String>,
    ) -> Result<Option<Select>> {
        let mut transaction = store.begin()?;
        let found = self.stored_view(&mut transaction, table);
        transaction.rollback();
        let Some(declared) = found? else {
            return Ok(None);
        };
        let mut read = parse_read(&declared).map_err(|why| Error::ViewUnreadable {
            name: table.name.text.clone(),
            detail: why.to_string(),
            span: table.span,
        })?;
        chain.push(table.name.text.clone());
        // A view's own read may name a view, so the expansion recurses — and the
        // depth check at the head of `expand_read` is what stops a chain, a
        // cycle included.
        self.expand_read(store, &mut read, chain)?;
        chain.pop();
        Ok(Some(read))
    }

    /// The stored read of a view, or `None` when this name is not one.
    fn stored_view(
        &self,
        transaction: &mut tessari_storage::Transaction<'_>,
        table: &TableRef,
    ) -> Result<Option<String>> {
        let qualified = table.database.as_ref().map(|name| name.text.as_str());
        let Ok(context) = self.context(transaction, qualified, table.span) else {
            return Ok(None);
        };
        let Some(id) = Catalog::new(transaction).table_id(
            context.namespace,
            context.database,
            &table.name.text,
        )?
        else {
            return Ok(None);
        };
        Ok(match Catalog::new(transaction).table(id)? {
            Some(definition) => match definition.kind {
                TableKind::View(declared) => Some(declared.read),
                _ => None,
            },
            None => None,
        })
    }
}
