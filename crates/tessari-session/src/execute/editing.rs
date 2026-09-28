//! What an edit does to a record, and the small conversions the statements share.

use std::collections::BTreeMap;
use tessari_encoding::Roles;
use tessari_ql::{Answer, Assignment, Edit, FieldPath, Name, Span};
use tessari_storage::{RecordAddress, Transaction, Violation};

use tessari_types::{Path, RecordId, Step, Value};

use crate::error::{Error, Result};
use crate::outcome::Outcome;
use crate::session::Session;

impl Session<'_> {
    /// The record a field-level `UPDATE` produces.
    ///
    /// # Every right-hand side sees the record as it was
    ///
    /// All of them are evaluated **before** any of them is applied, so
    /// `SET a = b, b = a` swaps rather than assigning `b` to both. It is SQL's
    /// rule and it is the only one that fits in a sentence; a left-to-right rule
    /// would make the meaning of a statement depend on the order somebody
    /// happened to type its clauses in.
    ///
    /// # Assigning `none` removes the field
    ///
    /// `Value::None` means the field is not there, so writing it into the object
    /// would say the field is there and holds not-being-there — the contradiction
    /// the value system spends its own rules avoiding, and the one a projection
    /// already refuses to produce.
    ///
    /// # A missing intermediate is refused, never created
    ///
    /// `SET a.b.c = 1` on a record with no `a` is an error naming the route.
    /// Creating the objects would be the store writing structure nobody asked
    /// for — the same call this store makes about zero-filling a hole in a file.
    /// The record an edit produces, whichever of the three shapes it is.
    ///
    /// Shared by `UPDATE` and `UPSERT` so the two cannot drift: the only thing
    /// that separates them is what they assert about the record beforehand, and
    /// a second copy of this match is how that stops being true.
    pub(super) fn applied(
        &self,
        transaction: &mut Transaction<'_>,
        edit: &Edit,
        existing: Value,
        span: Span,
    ) -> Result<(Value, Option<PartialSeal>)> {
        // An edit that computes from the record cannot compute from a vault's,
        // and this is where that is said. `SET` and `MERGE` build on `existing`;
        // in a vault `existing` holds the store's own `#keys` map and the
        // *ciphertext* of every sealed field, so the write that followed refused
        // with `VaultReservedField` — naming a field the caller never wrote and
        // cannot see — or, in a vault with a second secret, with a schema
        // violation saying a `string` field held bytes.
        //
        // Neither refusal was wrong about the write; both were unreadable about
        // the cause. And the cause is not a defect to route around: computing
        // from a sealed field means opening it, opening one is `REVEAL`, and
        // `REVEAL` writes an audit entry before it answers. An `UPDATE` that
        // quietly opened three secrets to re-seal them would put plaintext in
        // this process with nothing anywhere recording that it was there.
        //
        // So the whole record is the unit of a vault write, which is what
        // `UPDATE … = { … }` already is.
        let sealed_record = matches!(
            &existing,
            Value::Object(fields) if fields.contains_key(tessari_storage::KEYS_FIELD)
        );
        match edit {
            // Replacing the whole record is a write like a create, so the
            // defaults apply to it the same way — and so does the fresh data
            // key, which is why `= { … }` still clears the recipient set while
            // the two edits below no longer do.
            Edit::Whole(value) => Ok((self.evaluate(transaction, value)?, None)),
            Edit::Fields(assignments) if sealed_record => {
                let (base, keys) = without_the_key_set(existing);
                let named = assignments
                    .iter()
                    .map(|assignment| assignment.route.path.root().to_owned())
                    .collect();
                let payload = self.edited(transaction, base, assignments, span, true)?;
                Ok((payload, Some(PartialSeal { keys, named })))
            }
            Edit::Fields(assignments) => Ok((
                self.edited(transaction, existing, assignments, span, false)?,
                None,
            )),
            Edit::Merge(value) => {
                // The value position, like every other object literal — see
                // `Edit::Merge`. Computing from the record is `SET`'s job, which
                // is also why `MERGE` needs no restriction on a vault: it never
                // reads the record in the first place.
                let incoming = self.evaluate(transaction, value)?;
                let Value::Object(supplied) = &incoming else {
                    return Err(Error::MergeIsNotAnObject {
                        found: incoming.type_name(),
                        span,
                    });
                };
                if sealed_record {
                    let named = supplied.keys().cloned().collect();
                    let (base, keys) = without_the_key_set(existing);
                    return Ok((merged(base, incoming), Some(PartialSeal { keys, named })));
                }
                Ok((merged(existing, incoming), None))
            }
        }
    }

    pub(super) fn edited(
        &self,
        transaction: &mut Transaction<'_>,
        existing: Value,
        assignments: &[Assignment],
        span: Span,
        sealed: bool,
    ) -> Result<Value> {
        let mut record = existing;
        let mut wanted = Vec::with_capacity(assignments.len());
        for assignment in assignments {
            // On a vault, the assignment is evaluated against **no record**, and
            // the evaluator is then its own detector for an expression that
            // reads one. The alternative — walking the expression looking for
            // field references — is the piece that would be incomplete by
            // construction, and its incompleteness would evaluate a secret to
            // its ciphertext rather than refusing.
            let scope = if sealed {
                crate::evaluate::Scope::none()
            } else {
                crate::evaluate::Scope::of(&record)
            };
            let held = self
                .evaluate_in(transaction, &assignment.value, scope)
                .map_err(|error| match error {
                    Error::NoRecordInScope { .. } if sealed => {
                        Error::VaultEditComputesFromTheRecord { span }
                    }
                    other => other,
                })?;
            wanted.push(held);
        }

        if !matches!(record, Value::Object(_)) {
            // A record that is not an object has no named fields to change. The
            // key-value model stores single values that way (ADR-0010), and
            // `SET` is the verb for those.
            return Err(Error::NoSuchRouteToAssign {
                route: assignments
                    .first()
                    .map_or_else(String::new, |first| first.route.path.to_string()),
                span,
            });
        }
        for (assignment, held) in assignments.iter().zip(wanted) {
            let route = &assignment.route.path;
            let steps = route.steps();
            let Some((last, above)) = steps.split_last() else {
                // A bare field name: the root of the route is the field.
                set_field(&mut record, route.root(), held, &assignment.route, span)?;
                continue;
            };
            let Step::Field(name) = last else {
                // A position or `[*]`: assigning into an array by index is its
                // own question and `[*]` has three contexts, none of them this.
                return Err(Error::NoSuchRouteToAssign {
                    route: route.to_string(),
                    span: assignment.route.span,
                });
            };
            let parent = Path::new(route.root().to_owned(), above.to_vec());
            let Some(target) = parent.resolve_mut(&mut record) else {
                return Err(Error::NoSuchRouteToAssign {
                    route: parent.to_string(),
                    span: assignment.route.span,
                });
            };
            set_field(target, name, held, &assignment.route, span)?;
        }
        Ok(record)
    }
}

/// The roles a list of words names, folded into one set.
///
/// Shared by `DEFINE NODE` and `DEFINE REPLICA` because they name the same field
/// on the same membership row (ADR-0018 §2) from the two sides — this node, and
/// a peer. A second copy would not fail to compile if it drifted; it would
/// change which words a peer may be declared with, and only on the side nobody
/// was looking at.
///
/// An unrecognised word is refused rather than skipped: a role this build has no
/// name for is one the operator believes they set.
pub(super) fn named_roles(named: &[Name]) -> Result<Roles> {
    named.iter().try_fold(Roles::NONE, |carried, role| {
        Roles::parse(&role.text)
            .map(|found| carried.and(found))
            .ok_or(Error::Unknown {
                entity: "role",
                name: role.text.clone(),
                span: role.span,
            })
    })
}

/// The outcome a write reports, given what it was asked to answer with.
///
/// One place rather than four, so that the four writes cannot come to disagree
/// about what `AFTER` means. `Nothing` is the default and stays `Done`: a write
/// that answered with a record by default would make every caller pay to ship
/// back a value most of them already have.
/// The identity an edge record is written under.
///
/// One function rather than the formula repeated at each site, because `RELATE`
/// writes it and `DELETE a->e->b` has to derive the *same* string to find what
/// was written. Two copies that drift do not fail: the delete addresses a key
/// nothing is under, removes nothing, and reports success.
///
/// It is derived rather than supplied so that relating the same pair twice
/// replaces one record instead of adding a second — which is what makes `RELATE`
/// idempotent and keeps a node's adjacency a set.
pub(super) fn edge_identity(out: &RecordAddress, into: &RecordAddress) -> RecordId {
    RecordId::from(format!(
        "{}:{}->{}:{}",
        out.table, out.id, into.table, into.id
    ))
}

pub(super) fn answered(answer: Answer, before: Value, after: Value) -> Outcome {
    match answer {
        Answer::Nothing => Outcome::Done,
        Answer::Before => Outcome::Value(before),
        Answer::After => Outcome::Value(after),
    }
}

/// What a partial vault edit hands the sealer.
///
/// Two facts the storage layer cannot work out for itself: the record's own key
/// set, read from the store rather than from anything a caller wrote, and the
/// names the edit supplied. Everything not named is already an envelope.
pub(super) struct PartialSeal {
    /// The record's `#keys`, exactly as it was stored.
    pub(super) keys: Value,
    /// The fields this edit wrote, and therefore the only ones to seal.
    pub(super) named: std::collections::BTreeSet<String>,
}

/// Split a stored vault record into the part an edit works on and its key set.
///
/// The key set comes off because the payload an edit produces goes back through
/// the sealer, and the sealer refuses a payload carrying one — that refusal is
/// what stops a caller injecting a key map, and it is not weakened for the edit
/// path. The set travels beside the payload instead, in [`PartialSeal`].
pub(super) fn without_the_key_set(record: Value) -> (Value, Value) {
    let Value::Object(mut fields) = record else {
        return (record, Value::None);
    };
    let keys = fields
        .remove(tessari_storage::KEYS_FIELD)
        .unwrap_or(Value::None);
    (Value::Object(fields), keys)
}

/// Two records folded into one: `incoming` over `existing`.
///
/// Deep where **both** sides hold an object and total everywhere else. That rule
/// is the whole of it, and the shapes it settles are worth naming:
///
/// - object over object — merged, one level deeper;
/// - anything over anything else — the incoming value, whole. An array replaces
///   an array rather than concatenating or merging by position, because there is
///   no reading of "merge these two lists" that is right more often than it is
///   surprising;
/// - a field the incoming object does not name — left exactly as it was, which
///   is the point of the verb;
/// - an explicit `NULL` — written, because `NULL` is a value here and means
///   "known to be nothing". Removing a field is `SET route = NONE`, which says
///   removal out loud rather than hiding it inside a merge.
pub(super) fn merged(existing: Value, incoming: Value) -> Value {
    match (existing, incoming) {
        (Value::Object(mut into), Value::Object(from)) => {
            for (name, value) in from {
                let folded = match (into.remove(&name), value) {
                    (Some(held @ Value::Object(_)), value @ Value::Object(_)) => {
                        merged(held, value)
                    }
                    (_, value) => value,
                };
                into.insert(name, folded);
            }
            Value::Object(into)
        }
        // One side is not an object, so there is nothing to fold into: the
        // incoming value stands whole. The top level never reaches here — the
        // caller refuses a non-object there — but a route below it does, and
        // that is the "incoming wins" rule doing its job.
        (_, incoming) => incoming,
    }
}

/// Put a value into one field of an object, or take it out.
pub(super) fn set_field(
    holder: &mut Value,
    name: &str,
    held: Value,
    route: &FieldPath,
    span: Span,
) -> Result<()> {
    let Value::Object(fields) = holder else {
        return Err(Error::NoSuchRouteToAssign {
            route: route.path.to_string(),
            span,
        });
    };
    if held.is_present() {
        fields.insert(name.to_owned(), held);
    } else {
        fields.remove(name);
    }
    Ok(())
}

/// One disagreement, as the answer carries it.
///
/// The rule is a stable word and the detail is the store's own sentence, so a
/// caller scripting a repair matches on the first and shows the second — rather
/// than parsing a message written for a person, which is the thing that breaks
/// when the message is improved.
pub(super) fn violation_value(found: Violation) -> Value {
    Value::Object(BTreeMap::from([
        ("record".to_owned(), Value::String(found.record)),
        ("field".to_owned(), Value::String(found.field)),
        ("rule".to_owned(), Value::String(found.rule.to_owned())),
        ("detail".to_owned(), Value::String(found.detail)),
    ]))
}
