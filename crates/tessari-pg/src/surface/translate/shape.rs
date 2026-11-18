//! The clause layer: the SELECT itself — projection, FROM with its joins,
//! WHERE, GROUP BY, HAVING, ORDER BY, LIMIT — and the assembly of the store's
//! own statement text from what the clauses collected.

use super::parser::Parser;
use super::{Columns, Projection, Shaped, Translated, UNNAMED};
use super::{Refused, code};

/// What [`Parser::assemble`] hands back: the store script, the projection's
/// column shape, the record-name mapping, and the OFFSET the answer drops —
/// one struct instead of a tuple the lint reads as too complex.
struct Assembled {
    script: String,
    columns: Columns,
    record_names: Vec<Option<String>>,
    offset: u64,
}

impl Parser<'_> {
    pub(crate) fn select(&mut self) -> Result<Translated, Refused> {
        self.depth_guard()?;
        if self.keyword("SELECT").is_err() {
            return Err(Refused::surface(
                code::SYNTAX,
                "a query starts with SELECT in this build",
            ));
        }
        let shaped = self.shape()?;
        self.end_strict()?;
        let script = shaped.script.clone();
        Ok(Translated {
            script,
            parameters: std::mem::take(&mut self.parameters),
            statements: vec![shaped],
            slots: std::mem::take(&mut self.slots),
        })
    }

    /// The simple query path: statements until the text ends, the whole held
    /// in one implicit transaction block when there is more than one — the
    /// dialect's own multi-statement semantics (§54.2.2.1), on the store's
    /// own transaction boundary. Strings carrying transaction verbs take the
    /// `verbs` module's path instead, which dispatches every statement on one
    /// parser — see [`super::verbs`].
    pub(crate) fn select_many(&mut self) -> Result<Translated, Refused> {
        self.depth_guard()?;
        if self.keyword("SELECT").is_err() {
            return Err(Refused::surface(
                code::SYNTAX,
                "a query starts with SELECT in this build",
            ));
        }
        let mut statements = Vec::new();
        loop {
            statements.push(self.shape()?);
            if !self.end_more()? {
                break;
            }
            if self.keyword("SELECT").is_err() {
                return Err(Refused::surface(
                    code::SYNTAX,
                    "a query starts with SELECT in this build",
                ));
            }
        }
        let script = if statements.len() == 1 {
            statements[0].script.clone()
        } else {
            let mut script = String::from("BEGIN; ");
            for shaped in &statements {
                script.push_str(shaped.script.trim_end_matches(';'));
                script.push_str("; ");
            }
            script.push_str("COMMIT;");
            script
        };
        Ok(Translated {
            script,
            parameters: std::mem::take(&mut self.parameters),
            statements,
            slots: std::mem::take(&mut self.slots),
        })
    }

    /// One transaction verb at the cursor, when one opens here: the
    /// dialect's own words, run as the store's — `ROLLBACK` included, which
    /// the store spells `CANCEL`. `Ok(None)` is not a verb: the caller reads
    /// a query. Used by the `verbs` module, which drives strings mixing
    /// verbs with the other statements on one parser.
    pub(crate) fn transaction_verb(&mut self) -> Result<Option<Shaped>, Refused> {
        use super::write::Tag;
        let verb = if self.keyword("begin").is_ok() {
            Some(("BEGIN;", Tag::Begin))
        } else if self.keyword("start").is_ok() {
            if self.keyword("transaction").is_err() {
                return Err(Refused::surface(
                    code::SYNTAX,
                    "START wants TRANSACTION in this build",
                ));
            }
            Some(("BEGIN;", Tag::Begin))
        } else if self.keyword("commit").is_ok() || self.keyword("end").is_ok() {
            Some(("COMMIT;", Tag::Commit))
        } else if self.keyword("rollback").is_ok() || self.keyword("abort").is_ok() {
            Some(("CANCEL;", Tag::Rollback))
        } else {
            None
        };
        Ok(verb.map(|(script, tag)| Shaped {
            script: script.to_owned(),
            columns: Columns::Named(Vec::new()),
            table: None,
            catalog: None,
            inference: Vec::new(),
            record_names: Vec::new(),
            offset: 0,
            notice: None,
            tag,
            returning: None,
        }))
    }

    /// The SELECT is consumed; the rest of the shape follows. `IN (SELECT …)`
    /// recurses here, sharing the parameter and slot state with the outer
    /// query, so a literal in a subquery binds exactly where it stands.
    pub(crate) fn shape(&mut self) -> Result<Shaped, Refused> {
        let projection = self.projection()?;
        let mut from = None;
        let mut alias = None;
        let mut joins = String::new();
        if self.keyword("FROM").is_ok() {
            let at = self.snapshot();
            if let Some(word) = self.bare_ident()
                && super::catalog::opens(&word)
            {
                // A catalog statement: the responder answers it locally,
                // the projection's items become its column paths.
                return super::catalog::rest(self, projection.items, &word);
            }
            self.restore(at);
            from = Some(self.table_source("a qualified name")?);
            if self.keyword("AS").is_ok() {
                alias = Some(self.ident()?);
            } else {
                alias = self.bare_alias();
            }
            while let Some(words) = self.join_kind() {
                joins.push(' ');
                joins.push_str(&words);
                joins.push(' ');
                joins.push_str(&self.table_source("a subquery as a join source")?);
                if self.keyword("ON").is_err() {
                    return Err(Refused::surface(code::SYNTAX, "a join wants ON"));
                }
                let on = self.or()?;
                joins.push_str(" ON ");
                joins.push_str(&on);
            }
        }
        let condition = if self.keyword("WHERE").is_ok() {
            Some(self.or()?)
        } else {
            None
        };
        let group = if self.keyword("GROUP").is_ok() {
            if self.keyword("BY").is_err() {
                return Err(Refused::surface(code::SYNTAX, "GROUP wants BY"));
            }
            let mut text = self.ident_path()?;
            while self.comma().is_ok() {
                text.push_str(", ");
                text.push_str(&self.ident_path()?);
            }
            Some(text)
        } else {
            None
        };
        let having = if self.keyword("HAVING").is_ok() {
            let was = std::mem::replace(&mut self.inline_literals, true);
            let condition = self.or();
            self.inline_literals = was;
            Some(condition?)
        } else {
            None
        };
        let order = if self.keyword("ORDER").is_ok() {
            if self.keyword("BY").is_err() {
                return Err(Refused::surface(code::SYNTAX, "ORDER wants BY"));
            }
            let mut text = self.order_term()?;
            while self.comma().is_ok() {
                text.push_str(", ");
                text.push_str(&self.order_term()?);
            }
            Some(text)
        } else {
            None
        };
        let limit = if self.keyword("LIMIT").is_ok() {
            // `LIMIT ALL` is the dialect's no-bound spelling; a number is the
            // bound exactly as before.
            if self.keyword("ALL").is_err() {
                Some(self.number()?)
            } else {
                None
            }
        } else {
            None
        };
        let offset = if self.keyword("OFFSET").is_ok() {
            Some(self.number()?)
        } else {
            None
        };
        let assembled = self.assemble(
            projection, &from, alias, condition, group, having, order, limit, offset, joins,
        )?;
        Ok(Shaped {
            script: assembled.script,
            columns: assembled.columns,
            table: from,
            catalog: None,
            inference: std::mem::take(&mut self.inference),
            record_names: assembled.record_names,
            offset: assembled.offset,
            notice: None,
            tag: super::write::Tag::Select,
            returning: None,
        })
    }

    /// The clause words are consumed; the script is built. Kept separate from
    /// [`Self::shape`] so nothing here decides where the query ends — the
    /// caller owns that (the end of the text, or the subquery's close paren).
    /// `offset` rides only in the answer: the store is asked for the wider
    /// bounded read and the port drops the head of what it gets.
    #[allow(clippy::too_many_arguments)]
    fn assemble(
        &mut self,
        projection: Projection,
        from: &Option<String>,
        alias: Option<String>,
        condition: Option<String>,
        group: Option<String>,
        having: Option<String>,
        order: Option<String>,
        limit: Option<u64>,
        offset: Option<u64>,
        joins: String,
    ) -> Result<Assembled, Refused> {
        // Computed projections (CASE mappings, catalog function calls) are
        // the catalog's own: the store speaks neither, so they refuse by
        // name rather than half-running. A bare literal is the exception —
        // it binds as a parameter, which the store speaks natively.
        if projection
            .items
            .iter()
            .any(|item| matches!(&item.computed, Some(c) if !matches!(c, super::Computed::Lit(_))))
        {
            return Err(Refused::surface(
                code::FEATURE,
                "computed projections are not spoken over the store in this build",
            ));
        }
        // The store answers an unnamed arithmetic projection under the
        // port's own alias (e1, e2…), which the wire names ?column? —
        // the record name rides beside the client name.
        let record_names = projection
            .items
            .iter()
            .map(|item| {
                if item.column == super::UNNAMED {
                    item.text
                        .rsplit(" AS ")
                        .next()
                        .filter(|name| name.starts_with('e'))
                        .map(ToOwned::to_owned)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        let (script, columns) = match from {
            Some(table) => {
                let mut script = String::from("SELECT ");
                script.push_str(&projection.text);
                script.push_str(" FROM ");
                script.push_str(table);
                if let Some(alias) = &alias {
                    script.push_str(" AS ");
                    script.push_str(alias);
                }
                script.push_str(&joins);
                if let Some(condition) = condition {
                    script.push_str(" WHERE ");
                    script.push_str(&condition);
                }
                if let Some(group) = group {
                    script.push_str(" GROUP BY ");
                    script.push_str(&group);
                }
                if let Some(having) = having {
                    script.push_str(" HAVING ");
                    script.push_str(&having);
                }
                if let Some(order) = order {
                    script.push_str(" ORDER BY ");
                    script.push_str(&order);
                }
                // The wider read: limit + offset rows, so the answer after the
                // head drop holds at most `limit`. A sum that does not fit is
                // the refusal the number position already names. OFFSET alone
                // asks for no store bound — the unbounded ordered read.
                let bound = match (limit, offset) {
                    (Some(limit), Some(offset)) => {
                        Some(limit.checked_add(offset).ok_or_else(|| {
                            Refused::surface(code::SYNTAX, "the number does not fit")
                        })?)
                    }
                    (limit, None) => limit,
                    (None, Some(_)) => None,
                };
                if let Some(bound) = bound {
                    script.push_str(&format!(" LIMIT {bound}"));
                }
                script.push(';');
                (script, projection.columns)
            }
            None => {
                if condition.is_some()
                    || !joins.is_empty()
                    || group.is_some()
                    || having.is_some()
                    || order.is_some()
                    || limit.is_some()
                    || offset.is_some()
                {
                    return Err(Refused::surface(
                        code::SYNTAX,
                        "the clauses want a FROM in this build",
                    ));
                }
                let mut script = String::from("RETURN ");
                script.push_str(&projection.text);
                script.push(';');
                (script, Columns::Named(vec![UNNAMED.to_owned()]))
            }
        };
        Ok(Assembled {
            script,
            columns,
            record_names,
            offset: offset.unwrap_or(0),
        })
    }

    /// One table name, or a named refusal when what follows is the feature
    /// this build does not speak (a qualified name, a subquery in the source).
    fn table_source(&mut self, feature: &str) -> Result<String, Refused> {
        if self.peek_paren() {
            return Err(Refused::with_detail(
                code::FEATURE,
                "this build does not speak the source",
                feature.to_owned(),
            ));
        }
        let table = self.ident()?;
        let at = self.at;
        if self.dot() {
            return Err(Refused::surface(
                code::FEATURE,
                "one table is read at a time in this build; a qualified name is not spoken",
            ));
        }
        self.at = at;
        Ok(table)
    }

    /// The join words at the cursor, or `None` when no join follows: `JOIN`,
    /// `INNER JOIN`, and the three outer directions, `OUTER` allowed after a
    /// direction and changing nothing, as the store's own grammar says.
    fn join_kind(&mut self) -> Option<String> {
        let at = self.at;
        let direction = if self.keyword("LEFT").is_ok() {
            "LEFT"
        } else if self.keyword("RIGHT").is_ok() {
            "RIGHT"
        } else if self.keyword("FULL").is_ok() {
            "FULL"
        } else if self.keyword("INNER").is_ok() {
            "INNER"
        } else {
            ""
        };
        if self.keyword("JOIN").is_err() {
            self.at = at;
            return None;
        }
        let mut text = String::new();
        if !direction.is_empty() {
            text.push_str(direction);
            if direction != "INNER" && self.keyword("OUTER").is_ok() {
                text.push_str(" OUTER");
            }
            text.push(' ');
        }
        text.push_str("JOIN");
        Some(text)
    }

    /// One ORDER BY term: a field path, the direction, and where the absences
    /// sort — the store's own `NULLS FIRST`/`NULLS LAST` ride unchanged.
    fn order_term(&mut self) -> Result<String, Refused> {
        let mut text = self.ident_path()?;
        if self.keyword("DESC").is_ok() {
            text.push_str(" DESC");
        } else {
            let _ = self.keyword("ASC");
        }
        if self.keyword("NULLS").is_ok() {
            if self.keyword("FIRST").is_ok() {
                text.push_str(" NULLS FIRST");
            } else if self.keyword("LAST").is_ok() {
                text.push_str(" NULLS LAST");
            } else {
                return Err(Refused::surface(code::SYNTAX, "NULLS wants FIRST or LAST"));
            }
        }
        Ok(text)
    }

    /// One statement ends here; a trailing semicolon or whitespace is the
    /// dialect's own punctuation and is accepted, nothing more — what follows
    /// a semicolon is another statement, and the extended protocol speaks one
    /// per Parse.
    fn end_strict(&mut self) -> Result<(), Refused> {
        self.skip_space();
        if self.byte(b';') {
            self.skip_space();
            if self.at != self.source.len() {
                return Err(Refused::surface(
                    code::SYNTAX,
                    "cannot insert multiple commands into a prepared statement",
                ));
            }
            return Ok(());
        }
        self.end_here()
    }

    /// Whether another statement follows the semicolon just eaten: `true`
    /// when text remains, `false` at the end of the query.
    pub(crate) fn end_more(&mut self) -> Result<bool, Refused> {
        self.skip_space();
        if self.byte(b';') {
            self.skip_space();
            return Ok(self.at != self.source.len());
        }
        self.end_here()?;
        Ok(false)
    }

    fn end_here(&mut self) -> Result<(), Refused> {
        if self.at == self.source.len() {
            Ok(())
        } else {
            Err(Refused::with_detail(
                code::SYNTAX,
                "syntax error at or near the rest of the query",
                format!(
                    "at byte {}: {:?}",
                    self.at,
                    String::from_utf8_lossy(&self.source[self.at..])
                ),
            ))
        }
    }
}
