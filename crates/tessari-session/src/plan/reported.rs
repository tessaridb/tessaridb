//! The structure both `EXPLAIN` and an answer report.
//!
//! # One type, because two renderings of the same idea drift
//!
//! Until this existed, `EXPLAIN` built a `BTreeMap` by hand and an answer
//! carried a bare [`AccessPath`]. The two described the same read in different
//! words: a traversal explained as `graph` and answered `index`, a join
//! explained as `join` and answered `index` or `scan`, a materialised source
//! explained as `materialised` and answered whatever the *inner* read had done.
//! Nothing was wrong in either, and together they made the plan unusable — a
//! reader comparing what a statement said it would do against what it did was
//! comparing two vocabularies.
//!
//! So there is one type and one renderer, and the read and the planner both
//! fill it. Where they still differ is the one place they *should*: the planner
//! cannot know whether an ordered index will fill the statement's bound, so a
//! read that falls back reports the path it took and says so in a note.
//!
//! # Every field is one the planner can know without reading
//!
//! No invented cost. A number this store cannot know is a number it will not
//! print, and a plan carrying a made-up estimate is how somebody comes to trust
//! one. The candidate-to-result ratio — the number that says whether an index is
//! actually *working* — needs the read to have happened, so it is not here.

use std::collections::BTreeMap;

use tessari_types::{Number, Value};

use crate::outcome::{AccessPath, Exactness};

/// What a read does, or what it would do.
///
/// Built once per read and once per `EXPLAIN`, from the same enumeration and the
/// same choice. Only [`Self::access`] is always known; the rest describe a
/// choice that a scan, a traversal or a join never made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// How the records are reached.
    pub access: AccessPath,
    /// Where a source that names no table reads from.
    ///
    /// Only `node` today — the one value out of `meta` that has no table, no
    /// index and no choice.
    pub source: Option<&'static str>,
    /// The table read, when the source names one.
    pub table: Option<String>,
    /// The index that served the read, by name.
    pub index: Option<String>,
    /// What shape of test the index answered.
    pub shape: Option<&'static str>,
    /// How many of the index's fields the lookup fixes.
    ///
    /// A separate number rather than a second `shape` word, because the shape
    /// ordering is the ranking's tie-break and a new variant would move plans
    /// this is only reporting on. Equal to the index's arity is a complete
    /// lookup.
    pub columns: Option<u64>,
    /// How much of the key space a region read will touch.
    ///
    /// The one cost of it a plan can know without running it: each cell is a
    /// scan plus a lookup per level above it.
    pub cells: Option<u64>,
    /// The ceiling the choice promises, where it promises one.
    pub at_most: Option<u64>,
    /// Whether the records are provably the ones the question names.
    ///
    /// Derived from [`Self::access`] rather than set here, so that `EXPLAIN` and
    /// the read cannot disagree about it and a path added later cannot forget
    /// it. See [`AccessPath::exactness`].
    pub exact: Exactness,
}

impl Plan {
    /// A plan that is only its access path — a scan, a walk, a join.
    #[must_use]
    pub const fn new(access: AccessPath) -> Self {
        Self {
            access,
            source: None,
            table: None,
            index: None,
            shape: None,
            columns: None,
            cells: None,
            at_most: None,
            exact: access.exactness(),
        }
    }

    /// The same plan, over a named table.
    #[must_use]
    pub fn on(self, table: &str) -> Self {
        Self {
            table: Some(table.to_owned()),
            ..self
        }
    }

    /// The plan as the value `EXPLAIN` answers with, and the one an answer
    /// carries.
    ///
    /// Absent rather than null for a field this read had no answer for, so the
    /// object a scan produces is the two keys it actually knows rather than
    /// eight keys of which six say nothing.
    ///
    /// **`exact` is the one exception and the exception is the point.** It is
    /// written on every plan, including — especially — when it is `true`. A
    /// field that appears only when it is interesting teaches a reader that its
    /// absence means the dull value, and here the dull value is a claim: that
    /// the answer is provably the one the question names. That inference is
    /// exactly what a returned exactness exists to make unnecessary, so the
    /// field is never absent and never has to be inferred.
    ///
    /// `inexact` follows the ordinary rule, because it genuinely is absent
    /// rather than dull: an exact answer has no reason to give.
    #[must_use]
    pub fn to_value(&self) -> Value {
        let mut plan = BTreeMap::new();
        plan.insert("access".to_owned(), Value::from(self.access.name()));
        plan.insert("exact".to_owned(), Value::Bool(self.exact.is_exact()));
        if let Some(why) = self.exact.reason() {
            plan.insert("inexact".to_owned(), Value::from(why));
        }
        if let Some(source) = self.source {
            plan.insert("source".to_owned(), Value::from(source));
        }
        if let Some(table) = &self.table {
            plan.insert("table".to_owned(), Value::from(table.as_str()));
        }
        if let Some(index) = &self.index {
            plan.insert("index".to_owned(), Value::from(index.as_str()));
        }
        if let Some(shape) = self.shape {
            plan.insert("shape".to_owned(), Value::from(shape));
        }
        for (key, held) in [
            ("columns", self.columns),
            ("cells", self.cells),
            ("at_most", self.at_most),
        ] {
            if let Some(held) = held {
                plan.insert(
                    key.to_owned(),
                    Value::Number(Number::Integer(i64::try_from(held).unwrap_or(i64::MAX))),
                );
            }
        }
        Value::Object(plan)
    }
}
