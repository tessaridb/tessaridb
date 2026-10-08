//! `DEFINE PARAM` and `DROP PARAM` — a database's named values (ADR-0124 D2).
//!
//! The value is computed once, when the statement runs, and kept on the
//! database's catalog record, so it replicates, snapshots and restores with the
//! database. `OR REPLACE` changes it without touching anything that reads it.

use tessari_ql::{Expr, Span};
use tessari_storage::{Catalog, Transaction};

use crate::error::{Error, Result};
use crate::outcome::Outcome;
use crate::session::Session;

/// How a `DEFINE PARAM` treats a param that is already there.
pub(crate) struct Mode {
    /// `IF NOT EXISTS`: keep it.
    pub(crate) keep: bool,
    /// `OR REPLACE`: replace it.
    pub(crate) replace: bool,
}

impl Session<'_> {
    /// `DEFINE PARAM $name VALUE <expr>`.
    ///
    /// Answers the stored value, which the script runner writes into the
    /// statements after this one and then reports as done — the same way a
    /// `LET` reaches the statements below it.
    pub(crate) fn define_param(
        &self,
        transaction: &mut Transaction<'_>,
        name: &str,
        value: &Expr,
        mode: &Mode,
        span: Span,
    ) -> Result<Outcome> {
        let context = self.context(transaction, None, span)?;
        let Some(mut definition) = Catalog::new(transaction).database(context.database)? else {
            return Err(Error::NoDatabaseSelected { span });
        };
        if let Some(held) = definition.params.get(name)
            && !mode.replace
        {
            if mode.keep {
                return Ok(Outcome::Value(held.clone()));
            }
            return Err(Error::ParamExists {
                name: name.to_owned(),
                span,
            });
        }
        let computed = self.evaluate(transaction, value)?;
        definition.params.insert(name.to_owned(), computed.clone());
        Catalog::new(transaction).set_params(context.database, definition.params)?;
        Ok(Outcome::Value(computed))
    }

    /// `DROP PARAM $name`.
    pub(crate) fn drop_param(
        &self,
        transaction: &mut Transaction<'_>,
        name: &str,
        span: Span,
    ) -> Result<Outcome> {
        let context = self.context(transaction, None, span)?;
        let Some(mut definition) = Catalog::new(transaction).database(context.database)? else {
            return Err(Error::NoDatabaseSelected { span });
        };
        if definition.params.remove(name).is_none() {
            return Err(Error::Unknown {
                entity: "param",
                name: format!("${name}"),
                span,
            });
        }
        Catalog::new(transaction).set_params(context.database, definition.params)?;
        Ok(Outcome::Done)
    }
}
