//! Building a read.
//!
//! The clauses a read must have are carried in the type, and the ones it may
//! have are carried in the value. That split is the whole of the typestate here:
//! [`Select<NoSource>`] has no `build`, so a read with no `FROM` does not
//! compile, and everything else is an ordinary optional field.

use tessari_ql::{
    BinaryOp, Expr, ExprKind, FieldPath, Name, Ordering, Parameters, Projected, Projection, Script,
    Select as Syntax, Source, Span, Statement, StatementKind, TableRef,
};
use tessari_types::{Path, Step, Value};

use crate::{Error, Result};

/// The span a built node carries.
///
/// A built statement has no source text, so it has no offsets to report. It is
/// one value rather than an invented one per node: a synthetic offset would have
/// to be computed from the rendered text, which is the output of the very
/// rendering a round trip is testing.
const BUILT: Span = Span::new(0, 0);

/// A read that does not yet know what it reads.
///
/// Has no `build`: that is the point.
#[derive(Debug)]
pub struct NoSource;

/// A read that knows what it reads.
#[derive(Debug)]
pub struct Sourced(TableRef);

/// A read under construction.
#[derive(Debug)]
pub struct Select<State> {
    projection: Vec<Projected>,
    condition: Option<Expr>,
    order: Vec<Ordering>,
    start: Option<u64>,
    limit: Option<u64>,
    parameters: Parameters,
    bound: usize,
    /// The first thing the caller got wrong, held until `build`.
    ///
    /// Reported at the end rather than at the call that caused it, because a
    /// builder is written as one chained expression and there is nowhere in the
    /// middle of one to handle a `Result`.
    failure: Option<Error>,
    source: State,
}

/// A built query: the syntax, and the values its parameters stand for.
///
/// Two fields rather than one string, which is the crate's reason to exist. The
/// script names parameters; the map holds what they are. Nothing here is text a
/// caller supplied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query {
    /// The statement, as syntax.
    pub script: Script,
    /// What each parameter stands for, by name without the marker.
    pub parameters: Parameters,
}

/// Begin a read. Answers with every field until a projection is named.
#[must_use]
pub fn select() -> Select<NoSource> {
    Select {
        projection: Vec::new(),
        condition: None,
        order: Vec::new(),
        start: None,
        limit: None,
        parameters: Parameters::new(),
        bound: 0,
        failure: None,
        source: NoSource,
    }
}

impl<State> Select<State> {
    /// Answer with the value at one route, under the route's own last name.
    ///
    /// Naming any route turns `SELECT *` into a named list. A route that is not
    /// a route, or that ends in a position, is refused — a position has no name
    /// of its own, and every spelling one could be given is a convention learned
    /// from a surprise.
    #[must_use]
    pub fn field(mut self, route: &str) -> Self {
        match projected(route) {
            Ok(projected) => self.projection.push(projected),
            Err(error) => self.remember(error),
        }
        self
    }

    /// Sort by a route, ascending unless `descending`.
    #[must_use]
    pub fn order_by(mut self, route: &str, descending: bool) -> Self {
        match path(route) {
            Ok(route) => self.order.push(Ordering {
                key: expr(ExprKind::Path(route)),
                descending,
            }),
            Err(error) => self.remember(error),
        }
        self
    }

    /// Pass over this many records before answering.
    #[must_use]
    pub const fn start(mut self, records: u64) -> Self {
        self.start = Some(records);
        self
    }

    /// Answer with at most this many records.
    #[must_use]
    pub const fn limit(mut self, records: u64) -> Self {
        self.limit = Some(records);
        self
    }

    /// Keep only the records a route compares to a value the way it is asked to.
    ///
    /// The value never reaches the query text. It becomes a parameter, and the
    /// parameter's value goes in the map — which is why this method takes a
    /// [`Value`] and not a string of TessariQL.
    ///
    /// Several filters compose with `AND`, in the order they were written.
    #[must_use]
    pub fn filter(mut self, route: &str, op: BinaryOp, value: impl Into<Value>) -> Self {
        let route = match path(route) {
            Ok(route) => route,
            Err(error) => {
                self.remember(error);
                return self;
            }
        };
        let name = format!("p{}", self.bound);
        self.bound = self.bound.saturating_add(1);
        self.parameters.insert(name.clone(), value.into());
        let test = expr(ExprKind::Binary {
            op,
            left: Box::new(expr(ExprKind::Path(route))),
            right: Box::new(expr(ExprKind::Parameter(name))),
        });
        self.condition = Some(match self.condition.take() {
            None => test,
            Some(held) => expr(ExprKind::And(Box::new(held), Box::new(test))),
        });
        self
    }

    /// Hold the first failure, so a later one cannot hide it.
    fn remember(&mut self, error: Error) {
        if self.failure.is_none() {
            self.failure = Some(error);
        }
    }
}

impl Select<NoSource> {
    /// Read a table. This is what makes the read buildable.
    #[must_use]
    pub fn from(mut self, table: &str) -> Select<Sourced> {
        let named = match name(table) {
            Ok(named) => named,
            Err(error) => {
                self.remember(error);
                // Stands in for a table the caller never validly named. It is
                // never rendered: `build` reports the failure above first.
                Name {
                    text: String::new(),
                    span: BUILT,
                }
            }
        };
        let table = TableRef {
            database: None,
            name: named,
            span: BUILT,
        };
        Select {
            projection: self.projection,
            condition: self.condition,
            order: self.order,
            start: self.start,
            limit: self.limit,
            parameters: self.parameters,
            bound: self.bound,
            failure: self.failure,
            source: Sourced(table),
        }
    }
}

impl Select<Sourced> {
    /// The finished query: the syntax, and the values beside it.
    ///
    /// # Errors
    ///
    /// The first [`Error`] the chain recorded — a route that is not a route, or
    /// a name that is not a name.
    pub fn build(self) -> Result<Query> {
        if let Some(failure) = self.failure {
            return Err(failure);
        }
        let Sourced(table) = self.source;
        let from = match self.condition {
            None => Source::Table(table),
            Some(condition) => Source::Where {
                table,
                condition: Box::new(condition),
            },
        };
        let projection = if self.projection.is_empty() {
            Projection::All
        } else {
            // `None`: the builder has no star to write. A caller that wants the
            // record whole leaves the projection empty, which is `All` above.
            Projection::Values {
                everything: None,
                values: self.projection,
            }
        };
        let select = Syntax {
            projection,
            // The builder offers no `OMIT`: it subtracts from a star this API
            // has no way to write.
            omit: Vec::new(),
            from,
            // Nor `ONLY`: it is an assertion about how many records answer, and
            // a builder cannot make one on the caller's behalf.
            only: None,
            fetch: Vec::new(),
            // Nor `SPLIT ON`: it changes how many records answer, which is a
            // question the caller asks in the language rather than a shape a
            // builder assembles.
            split: None,
            group: Vec::new(),
            order: self.order,
            // Nor a cursor: `AFTER` anchors a page on a record the caller read
            // out of a previous answer, and a builder that has not seen an
            // answer has no anchor to offer.
            after: None,
            approximate: None,
            start: self.start,
            limit: self.limit,
            // The builder states no expectation about the path. An assertion is
            // something an author writes on purpose, and a builder that carried
            // one by default would refuse reads nobody asked it to police.
            using: None,
            // And no ceiling, for the same reason: a budget the caller did not
            // ask for is a refusal the caller did not ask for.
            timeout: None,
            // And no version: the builder reads the present. Naming a point in
            // the store's history means holding a sequence read out of a
            // previous answer, which is the same thing that keeps `AFTER` off
            // this API — a builder that has not seen an answer has nothing to
            // name.
            version: None,
            span: BUILT,
        };
        Ok(Query {
            script: Script {
                statements: vec![Statement {
                    kind: StatementKind::Select(Box::new(select)),
                    span: BUILT,
                }],
                span: BUILT,
            },
            parameters: self.parameters,
        })
    }
}

/// An expression at a built node's span.
fn expr(kind: ExprKind) -> Expr {
    Expr { kind, span: BUILT }
}

/// One projected route, under the name its last step gives it.
fn projected(route: &str) -> Result<Projected> {
    let route = path(route)?;
    let text = match route.path.steps().last() {
        None => route.path.root().to_owned(),
        Some(Step::Field(named)) => named.clone(),
        // A position and `[*]` are un-nameable, which is the parser's rule and
        // not a limitation invented here.
        Some(Step::Index(_) | Step::Every) => {
            return Err(Error::NotAName {
                text: route.path.to_string(),
            });
        }
    };
    Ok(Projected {
        value: expr(ExprKind::Path(route)),
        name: Name { text, span: BUILT },
    })
}

/// A route into a record, with every name segment checked.
///
/// The check is what stops a route being a way in. `Path::parse` accepts any
/// text that is not a delimiter, so without this a route could carry a whole
/// clause into the rendered query — the one place in this crate where caller
/// text becomes grammar.
fn path(route: &str) -> Result<FieldPath> {
    let parsed = Path::parse(route).ok_or_else(|| Error::NotARoute {
        text: route.to_owned(),
    })?;
    name(parsed.root())?;
    for step in parsed.steps() {
        match step {
            Step::Field(segment) => {
                name(segment)?;
            }
            Step::Index(_) | Step::Every => {}
        }
    }
    Ok(FieldPath {
        path: parsed,
        span: BUILT,
    })
}

/// A bare identifier, or a refusal naming what was supplied.
fn name(text: &str) -> Result<Name> {
    let first_is_digit = text.starts_with(|character: char| character.is_ascii_digit());
    let valid = !text.is_empty()
        && !first_is_digit
        && text
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_');
    if valid {
        Ok(Name {
            text: text.to_owned(),
            span: BUILT,
        })
    } else {
        Err(Error::NotAName {
            text: text.to_owned(),
        })
    }
}
