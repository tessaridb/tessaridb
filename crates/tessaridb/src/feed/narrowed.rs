//! A feed narrowed by a condition (ADR-0122 Part B).
//!
//! # A mirror holds exactly the matching set
//!
//! A write that matches is delivered. A write that does not match, and a
//! removal, are delivered **as a removal** when the record matched just before
//! — so a subscriber applying the feed drops a record the moment it leaves,
//! and is told nothing about a record it never held. "Just before" is the
//! version under the change's own commit order, read the way `VERSION` reads;
//! when that version has been reclaimed the feed ends rather than guessing.
//!
//! # One value answers the permission and the filter
//!
//! The condition is judged on the record as the subscriber may see it, and a
//! condition naming a field they may not see is refused — at open, and at the
//! first round after a grant takes the field away. Judged on the stored record
//! instead, the condition would be an oracle for the hidden field: which
//! records arrive would say what it holds.

use std::collections::BTreeSet;
use std::time::Instant;

use tessari_ql::{Expr, ExprKind, Parameters};
use tessari_session::redact::Visible;
use tessari_storage::{Change, ChangeKind, RecordAddress, Transaction};
use tessari_types::{Sequence, Value};

use super::{FeedRefused, PATIENCE};
use crate::{Db, Session};

/// The condition a subscriber narrows a feed by: TessariQL text, and the
/// values its parameters are bound to after it is read.
#[derive(Debug, Clone, Copy)]
pub struct Condition<'a> {
    /// The condition, without `WHERE`.
    pub text: &'a str,
    /// What its `$name`s stand for.
    pub parameters: &'a Parameters,
}

/// How far a narrowed feed has read, sent when it skipped changes and
/// delivered nothing: resuming after it repeats nothing and loses nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Progress {
    /// The sequence of the last change read — resume at one more.
    pub sequence: Sequence,
    /// On a feed over a split table, the cursor to resume after it.
    pub cursor: Option<String>,
}

/// What a change is to a narrowed feed.
pub(super) enum Verdict {
    /// It matches: deliver it as it is.
    Deliver,
    /// It left the matching set, or was removed while in it.
    Removal,
    /// It says nothing about a record the subscriber holds.
    Skip,
}

/// A condition read, bound and checked, and what the feed has said lately.
pub(super) struct Narrowed {
    condition: Expr,
    /// The fields it reads.
    fields: BTreeSet<String>,
    /// Pending progress: the last change skipped since anything was said.
    held: Option<Progress>,
    /// When the subscriber was last told a position.
    said: Option<Instant>,
}

impl Narrowed {
    /// Read and bind `asked`, and refuse it if it cannot be judged from a
    /// change or names a field `visible` hides.
    ///
    /// # Errors
    ///
    /// The parse or bind failure, [`FeedRefused::ConditionReadsTheStore`], or
    /// [`FeedRefused::FieldNotVisible`].
    pub(super) fn open(asked: &Condition<'_>, visible: &Visible) -> Result<Self, FeedRefused> {
        let parsed =
            tessari_ql::parse_condition(asked.text).map_err(tessari_session::Error::from)?;
        let condition = tessari_ql::bind_expression(parsed, asked.parameters)
            .map_err(tessari_session::Error::from)?;
        let mut fields = BTreeSet::new();
        if !judged_alone(&condition, &mut fields) {
            return Err(FeedRefused::ConditionReadsTheStore);
        }
        let narrowed = Self {
            condition,
            fields,
            held: None,
            said: None,
        };
        narrowed.still_visible(visible)?;
        Ok(narrowed)
    }

    /// Refuse a condition reading a field `visible` hides.
    ///
    /// # Errors
    ///
    /// [`FeedRefused::FieldNotVisible`] naming the first such field.
    pub(super) fn still_visible(&self, visible: &Visible) -> Result<(), FeedRefused> {
        let Some(shown) = visible else {
            return Ok(());
        };
        match self.fields.iter().find(|field| !shown.contains(*field)) {
            Some(field) => Err(FeedRefused::FieldNotVisible {
                field: field.clone(),
            }),
            None => Ok(()),
        }
    }

    /// What `change` is to this feed.
    ///
    /// # Errors
    ///
    /// What judging the condition refuses, and
    /// [`FeedRefused::PreviousVersionGone`] when the version a non-matching
    /// change must be compared with is no longer held.
    pub(super) fn judge(
        &self,
        db: &Db,
        session: &Session<'_>,
        transaction: &mut Transaction<'_>,
        change: &Change,
        visible: &Visible,
    ) -> Result<Verdict, FeedRefused> {
        if let ChangeKind::Written(value) = &change.kind
            && session.holds_for(
                transaction,
                &self.condition,
                (&change.id, value.clone()),
                visible,
            )?
        {
            return Ok(Verdict::Deliver);
        }
        // A condition that reads no field cannot tell one version from
        // another, so whether the record matched before is whether it
        // matches now, and no version is read for it.
        let before = if self.fields.is_empty() {
            Some(Value::Object(std::collections::BTreeMap::new()))
        } else {
            let gone = || FeedRefused::PreviousVersionGone {
                sequence: change.sequence.get(),
            };
            let order = change.order.ok_or_else(gone)?;
            let Some(at) = order.get().checked_sub(1) else {
                return Ok(Verdict::Skip);
            };
            let address = RecordAddress::new(
                change.namespace,
                change.database,
                change.table,
                change.id.clone(),
            );
            match db.store().held_at(Sequence::new(at), &address) {
                Ok(held) => held,
                Err(tessari_storage::Error::VersionReclaimed { .. }) => return Err(gone()),
                Err(failure) => return Err(failure.into()),
            }
        };
        let matched = match before {
            Some(held) => {
                session.holds_for(transaction, &self.condition, (&change.id, held), visible)?
            }
            None => false,
        };
        Ok(if matched {
            Verdict::Removal
        } else {
            Verdict::Skip
        })
    }

    /// Something was delivered: the subscriber holds a position again.
    pub(super) fn delivered(&mut self) {
        self.held = None;
        self.said = Some(Instant::now());
    }

    /// A change was skipped; `resume` is the cursor after it, if any.
    pub(super) fn skipped(&mut self, change: &Change, resume: Option<&str>) {
        self.held = Some(Progress {
            sequence: change.sequence,
            cursor: resume.map(str::to_owned),
        });
    }

    /// The progress to send at `now`: only when changes were skipped since the
    /// subscriber was last told a position, and at most once per
    /// [`PATIENCE`].
    pub(super) fn progress(&mut self, now: Instant) -> Option<Progress> {
        let quiet = self
            .said
            .is_none_or(|said| said.checked_add(PATIENCE).is_some_and(|due| now >= due));
        if !quiet {
            return None;
        }
        let progress = self.held.take()?;
        self.said = Some(now);
        Some(progress)
    }
}

/// Whether `expr` can be judged from one change alone — nothing in it reads
/// the store — collecting the fields it reads into `fields`.
fn judged_alone(expr: &Expr, fields: &mut BTreeSet<String>) -> bool {
    let all = |items: &[Expr], fields: &mut BTreeSet<String>| {
        items.iter().all(|item| judged_alone(item, fields))
    };
    match &expr.kind {
        ExprKind::Path(field) => {
            fields.insert(field.path.root().to_owned());
            true
        }
        ExprKind::Literal(_) | ExprKind::Parameter(_) => true,
        ExprKind::Not(inner) | ExprKind::Negate(inner) | ExprKind::Route { value: inner, .. } => {
            judged_alone(inner, fields)
        }
        ExprKind::If {
            condition,
            then,
            otherwise,
        } => {
            judged_alone(condition, fields)
                && judged_alone(then, fields)
                && otherwise
                    .as_ref()
                    .is_none_or(|otherwise| judged_alone(otherwise, fields))
        }
        ExprKind::Coalesce(left, right)
        | ExprKind::And(left, right)
        | ExprKind::Or(left, right)
        | ExprKind::Arithmetic { left, right, .. }
        | ExprKind::Binary { left, right, .. } => {
            judged_alone(left, fields) && judged_alone(right, fields)
        }
        ExprKind::Call { arguments, .. } => all(arguments, fields),
        ExprKind::Array(items) | ExprKind::Set(items) => all(items, fields),
        ExprKind::Object(entries) => entries
            .iter()
            .all(|entry| judged_alone(&entry.value, fields)),
        // A record named in the condition is a value to compare with; reading
        // one, a table, a record's lifetime, a fold or a subquery is reading
        // the store, which no change carries.
        ExprKind::Record(_) => true,
        ExprKind::Range(_)
        | ExprKind::Select(_)
        | ExprKind::Get(_)
        | ExprKind::Ttl(_)
        | ExprKind::Table(_)
        | ExprKind::Fold { .. } => false,
    }
}
