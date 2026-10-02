//! A condition every disjunct of which an index serves (G051 T7.2).
//!
//! `title MATCHES q OR body MATCHES q` names two indexes and offers neither a
//! conjunct it can serve alone: a candidate from one side would leave out the
//! records only the other side reaches. So the enumeration, which works on
//! conjuncts, finds nothing and the read scans the table. Here each disjunct is
//! planned on its own, by the same enumeration and the same `serving`, and the
//! read is served by the union of their candidates — but only when **every**
//! disjunct has one worth serving, because a union missing a side is not a
//! superset of the answer.
//!
//! The union is candidates like any other. Every record in it is tested again
//! against the whole condition, so the answer is the scan's.

use tessari_ql::{Expr, ExprKind};
use tessari_storage::{IndexDefinition, Transaction};
use tessari_types::TableId;

use super::candidate::Candidate;
use super::reported::Plan;
use crate::error::Result;
use crate::outcome::AccessPath;
use crate::search::Searched;
use crate::session::Session;

impl Session<'_> {
    /// One candidate per disjunct of `condition`'s top-level `OR`, when every
    /// disjunct has one worth serving; `None` for a condition that is not a
    /// disjunction, or one with a side no index serves.
    pub(crate) fn union_of(
        &self,
        transaction: &mut Transaction<'_>,
        table: TableId,
        condition: &Expr,
        (declared, searched): (&[IndexDefinition], &Searched),
        lifted: bool,
    ) -> Result<Option<Vec<Candidate>>> {
        let mut disjuncts = Vec::new();
        disjuncts_of(condition, &mut disjuncts);
        if disjuncts.len() < 2 {
            return Ok(None);
        }
        let mut chosen = Vec::with_capacity(disjuncts.len());
        for disjunct in disjuncts {
            let offered = self.enumerate(transaction, disjunct, declared, searched)?;
            match super::serving(transaction, table, offered, lifted)? {
                Some(candidate) => chosen.push(candidate),
                None => return Ok(None),
            }
        }
        Ok(Some(chosen))
    }
}

/// The sides of a chain of `OR`s, in the order written.
fn disjuncts_of<'a>(condition: &'a Expr, into: &mut Vec<&'a Expr>) {
    match &condition.kind {
        ExprKind::Or(left, right) => {
            disjuncts_of(left, into);
            disjuncts_of(right, into);
        }
        _ => into.push(condition),
    }
}

/// What a read served by a union reports: every index it read, in the order
/// the disjuncts were written, under the shape `union`.
pub(crate) fn union_plan(chosen: &[Candidate], table: Option<&str>) -> Plan {
    let names: Vec<&str> = chosen
        .iter()
        .map(|candidate| candidate.index.name.as_str())
        .collect();
    Plan {
        table: table.map(ToOwned::to_owned),
        index: Some(names.join(", ")),
        shape: Some("union"),
        ..Plan::new(AccessPath::Index)
    }
}
