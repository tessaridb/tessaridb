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
//! one. The one estimate a plan does carry (G055 W3) is read from statistics
//! the index's own entries produced, or counted from them, and it is printed
//! with which of the two it was. The candidate-to-result ratio — the number
//! that says whether an index is actually *working* — needs the read to have
//! happened, so it is not here.

use std::collections::BTreeMap;

use tessari_types::{Number, Value};

use crate::outcome::{AccessPath, Exactness};

/// What a plan knows about how many records its index produces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expected {
    /// A ceiling that was free to learn — a unique equality, a term's document
    /// frequency. Reported as `at_most`.
    AtMost(u64),
    /// An estimate read from the index's statistics. Reported as `estimate`
    /// with `estimated_by: 'statistics'`.
    Estimated(u64),
    /// The entries a probe counted to decide whether the index beats the table.
    /// Reported as `estimate` with `estimated_by: 'probe'`.
    Counted(u64),
}

impl Expected {
    /// The number, whichever it is.
    #[must_use]
    pub const fn rows(self) -> u64 {
        match self {
            Self::AtMost(held) | Self::Estimated(held) | Self::Counted(held) => held,
        }
    }

    /// The estimate and its source, when this is one.
    ///
    /// An estimate is the one number in a plan that may be wrong, which is why
    /// it travels with where it came from. It is withheld when the index reads
    /// a field the caller may not see — see [`Plan::seen_by`].
    #[must_use]
    pub const fn estimate(self) -> Option<(u64, &'static str)> {
        match self {
            Self::Estimated(held) => Some((held, "statistics")),
            Self::Counted(held) => Some((held, "probe")),
            Self::AtMost(_) => None,
        }
    }
}

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
    /// How many records the choice expects its index to produce, and what
    /// said so.
    ///
    /// One field rather than a ceiling beside an estimate, because a candidate
    /// has at most one of them — and an answer carries a plan, so its size is
    /// paid by every answer the store gives.
    pub expected: Option<Expected>,
    /// The shards of a split table a span read touches, in key order
    /// (ADR-0096 D3) — so a read naming one partition says it reads one shard.
    pub shards: Option<Box<[u32]>>,
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
            expected: None,
            shards: None,
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

    /// The same plan, without its estimate when the index reads a field this
    /// caller may not see.
    ///
    /// Top-level, as a redaction is: a grant names a field of a table. A count
    /// of the records holding a value is a fact about that value, and a caller
    /// who cannot read the field must not learn it one `EXPLAIN` at a time.
    #[must_use]
    pub fn seen_by(
        self,
        index: &tessari_storage::IndexDefinition,
        visible: &crate::redact::Visible,
    ) -> Self {
        let hidden = visible.as_ref().is_some_and(|fields| {
            index
                .fields
                .iter()
                .any(|path| !fields.contains(path.root()))
        });
        match self.expected {
            Some(Expected::Estimated(_) | Expected::Counted(_)) if hidden => Self {
                expected: None,
                ..self
            },
            _ => self,
        }
    }

    /// The same plan, touching these shards.
    #[must_use]
    pub fn touching(self, shards: Option<Vec<u32>>) -> Self {
        Self {
            shards: shards.map(Vec::into_boxed_slice),
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
        let at_most = match self.expected {
            Some(Expected::AtMost(held)) => Some(held),
            _ => None,
        };
        for (key, held) in [
            ("columns", self.columns),
            ("cells", self.cells),
            ("at_most", at_most),
        ] {
            if let Some(held) = held {
                plan.insert(
                    key.to_owned(),
                    Value::Number(Number::Integer(i64::try_from(held).unwrap_or(i64::MAX))),
                );
            }
        }
        if let Some((rows, by)) = self.expected.and_then(Expected::estimate) {
            plan.insert(
                "estimate".to_owned(),
                Value::Number(Number::Integer(i64::try_from(rows).unwrap_or(i64::MAX))),
            );
            plan.insert("estimated_by".to_owned(), Value::from(by));
        }
        if let Some(shards) = &self.shards {
            plan.insert(
                "shards".to_owned(),
                Value::Array(
                    shards
                        .iter()
                        .map(|shard| Value::Number(Number::Integer(i64::from(*shard))))
                        .collect(),
                ),
            );
        }
        Value::Object(plan)
    }
}
