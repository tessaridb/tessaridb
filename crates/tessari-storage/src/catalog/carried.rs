//! Which subscribers a mutation is carried to.
//!
//! A subscription is a [`Reach`] (ADR-0061) and the stream is filtered by it:
//! the leader sends every sequence to every follower, and a mutation outside the
//! subscription's reach is elided rather than delivered. This module answers the
//! one question that makes that possible — *whose is this record?*
//!
//! # It is a pure function of the mutation, and that is the load-bearing choice
//!
//! Nothing here reads the store. A catalog lookup would be taken at **stream**
//! time rather than at the record's own time, so a table dropped after the
//! record was written would classify differently on a re-stream than on the
//! first pass — and a filter whose answer depends on when it ran is a filter
//! nobody can reason about. The price is paid openly below: what cannot be
//! proven from the mutation alone is [`Carried::StoreOnly`], which for a
//! selective follower means elided.
//!
//! # The identity class travels everywhere, always — and what had to be true first
//!
//! The cluster has **one set of users**: a node that joins holds the same
//! identities as the node it joined, so an operator who can sign in on the
//! leader can sign in on any follower. That is the concept's own text and it is
//! the instruction this module now follows.
//!
//! It has not always been safe, and the history is worth keeping because it is
//! the argument for the invariant that replaced it. This module once carried a
//! user by **the user's own tenancy**, and the reason was [`Kind::Replicate`]:
//! it had joined a closed set whose owner role expands to every kind, so every
//! namespace owner already declared held it over their own namespace. An
//! *everywhere* identity class would have handed them every credential hash in
//! the store, across every tenancy, through a permission nobody granted them
//! separately.
//!
//! What changed is the permission, not the appetite for the risk.
//! [`Kind::may_be_held_at`] makes `replicate` a thing held over the whole store
//! or not at all, so the only principal who can open **any** subscription is one
//! already entitled to every byte in the store. A follower is provisioned by the
//! cluster rather than asked for by a tenant, and the narrowing a selective
//! subscription performs is an arrangement rather than a right.
//!
//! # So the protection moved rather than being dropped
//!
//! It used to be *the hash does not arrive*. It is now *the hash arrives on a
//! node the cluster stood up, and a tenant on that node cannot read it* —
//! `Session::administers` bounds every read of a user by the reader's own
//! tenancy, on the singular form and on the listing alike. Those are different
//! defences with different failure modes, and the second is the one that has to
//! hold on a follower, so it is asserted there rather than inferred.
//!
//! One thing does not move: a hash that reached a subscriber has reached it, and
//! no later fix recovers that. The reason this direction is takeable at all is
//! that the set of subscribers shrank first.
//!
//! [`Kind::Replicate`]: super::Kind
//! [`Kind::may_be_held_at`]: super::Kind::may_be_held_at

use tessari_encoding::{LogRecord, Mutation, RecordValue, decode_payload};
use tessari_types::{DatabaseId, NamespaceId, RecordId, Value};

use super::system::{self, Level};
use super::{Reach, definition};
use crate::error::Result;

/// Who a mutation is carried to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Carried {
    /// It belongs to one tenancy, and travels to a subscription that reaches it.
    Within(Reach),
    /// It DEFINES something at this reach — a namespace, a database, a table, a
    /// name, a leadership — and travels to a subscription inside it as well as
    /// to one containing it (Q-788, G031 S3.2).
    ///
    /// Definitions travel down the containment order and data does not travel
    /// sideways. A subscriber to one database needs the namespace's definition
    /// and name to be able to say `USE NAMESPACE` at all, and a subscriber to one
    /// shard needs its table's; carried only `Within` their own level, a
    /// narrower subscription received records nothing on it could name.
    Schema(Reach),
    /// It has no tenancy of its own and holds nothing secret, and any tenancy's
    /// schema may point at it — so it travels to every subscriber.
    ///
    /// Analyzers: a name unique across the store, a filter chain, and no
    /// reference to anybody's data. Withholding one would leave a follower
    /// holding a full-text field whose analyzer it cannot resolve, which fails
    /// at read time and looks like corruption. And the identity class — users
    /// and the grants that narrow them, which must arrive together (see
    /// [`carried_to`]).
    Everywhere,
    /// It is the whole store's business, or its tenancy cannot be proven from
    /// the mutation alone. Either way it travels only to a subscriber at
    /// [`Reach::Store`].
    StoreOnly,
}

impl Carried {
    /// Whether a subscription over `subscription` receives this.
    pub(crate) fn reaches(self, subscription: Reach) -> bool {
        match self {
            Self::Everywhere => true,
            Self::Within(own) => subscription.contains(own),
            Self::Schema(own) => subscription.contains(own) || own.contains(subscription),
            Self::StoreOnly => subscription == Reach::Store,
        }
    }
}

/// Who this mutation is carried to.
///
/// # Errors
///
/// Returns [`crate::Error::CatalogMalformed`] when a catalog record is present
/// and cannot be decoded. A malformed definition is refused rather than treated
/// as untenanted: answering [`Carried::StoreOnly`] would silently withhold a
/// follower's own schema, and the store would be wrong in a direction nothing
/// reports.
pub(crate) fn carried_to(mutation: &Mutation) -> Result<Carried> {
    if mutation.namespace != system::SYSTEM_NAMESPACE
        || mutation.database != system::SYSTEM_DATABASE
    {
        // Ordinary data. Its address *is* its tenancy, so this costs no decode
        // at all — which is the case that runs on every record.
        //
        // A mutation of a split table carries its shard, stamped by the writer
        // at commit (G031, ADR-0080), so it is carried within that shard — read
        // from the record's own bytes, like everything else here.
        if let Some(shard) = mutation.shard {
            return Ok(Carried::Within(Reach::Shard(
                mutation.namespace,
                mutation.database,
                mutation.table,
                shard,
            )));
        }
        return Ok(
            match Reach::of(Some(mutation.namespace), Some(mutation.database)) {
                Some(reach) => Carried::Within(reach),
                // Unconstructible from a mutation, which always carries both.
                None => Carried::StoreOnly,
            },
        );
    }
    let present = match mutation.value.value() {
        RecordValue::Present(payload) => Some(decode_payload(payload)?),
        // A tombstone in the system tenancy has lost the fields its tenancy was
        // written in. Unprovable, so `StoreOnly` — a selective follower can
        // therefore keep a stale catalog entry for an object dropped in its own
        // namespace. Staleness is the safe side of this trade and disclosure is
        // not, which is the direction structural default-deny asks for.
        RecordValue::Tombstone => None,
    };
    let table = mutation.table;
    if table == system::NAMES {
        return Ok(named(mutation, present.as_ref()));
    }
    Ok(match (table, present) {
        // A synonym or stop-word set is a store-wide name like an analyzer, and
        // a search in any namespace may read it (ADR-0105).
        (t, _) if t == system::ANALYZERS || t == system::WORD_SETS => Carried::Everywhere,
        (t, Some(value)) if t == system::NAMESPACES => Carried::Schema(Reach::Namespace(
            definition::NamespaceDefinition::from_value(&value)?.id,
        )),
        (t, Some(value)) if t == system::DATABASES => {
            let declared = definition::DatabaseDefinition::from_value(&value)?;
            Carried::Schema(Reach::Database(declared.namespace, declared.id))
        }
        (t, Some(value)) if t == system::TABLES => {
            let declared = super::TableDefinition::from_value(&value)?;
            Carried::Schema(Reach::Database(declared.namespace, declared.database))
        }
        (t, Some(value)) if t == system::INDEXES => {
            let declared = definition::IndexDefinition::from_value(&value)?;
            Carried::Schema(Reach::Database(declared.namespace, declared.database))
        }
        (t, Some(value)) if t == system::FIELDS => {
            let declared = super::FieldDefinition::from_value(&value)?;
            Carried::Schema(Reach::Database(declared.namespace, declared.database))
        }
        (t, Some(value)) if t == system::GRAPHS => {
            let declared = super::GraphDefinition::from_value(&value)?;
            Carried::Schema(Reach::Database(declared.namespace, declared.database))
        }
        (t, Some(value)) if t == system::EDGE_KINDS => {
            let declared = super::EdgeKindDefinition::from_value(&value)?;
            Carried::Schema(Reach::Database(declared.namespace, declared.database))
        }
        (t, Some(value)) if t == system::CONSUMERS => Carried::Schema(Reach::Namespace(
            super::ConsumerDefinition::from_value(&value)?.namespace,
        )),
        // A leadership travels to whoever holds the range it is about: a
        // follower subscribed to one namespace needs to know who leads that
        // namespace, and has no business learning who leads another's.
        (t, Some(value)) if t == system::LEADERSHIPS => {
            Carried::Schema(super::LeadershipDefinition::from_value(&value)?.range)
        }
        // The identity class, and it is unconditional: every user reaches every
        // subscriber, whatever tenancy they were declared at. Not read from the
        // record at all, because there is no longer a question to ask of it —
        // classifying by tenancy is what produced the split this reversed, and
        // leaving the read in place would leave the split one edit away.
        (t, _) if t == system::USERS => Carried::Everywhere,
        // A user's grants travel with the user, for the same reason and with
        // the same unconditional rule (G033). A grant NARROWS its user — one
        // grant reduces a user to exactly what was granted — so a follower
        // holding the user without the grant holds that user UNRESTRICTED, and
        // a reader allowed one field reads the whole record there with nothing
        // in an error state. It was `StoreOnly` because a grant carries a table
        // id and no tenancy; that was right while users were classified by
        // tenancy too, and became a widening the day they travelled everywhere.
        // A revocation's tombstone reaches every node the grant did.
        (t, _) if t == system::GRANTS => Carried::Everywhere,
        // A revoked certificate reaches every node, whatever it is subscribed
        // to: a follower that never received the row keeps admitting the peer
        // every other node refuses, with nothing in an error state (ADR-0108 D6).
        (t, _) if t == system::REVOKED_CERTIFICATES => Carried::Everywhere,
        // And a removed node, for the same reason: a follower that missed it
        // would admit the node every other one refuses.
        (t, _) if t == system::TOMBSTONED_NODES => Carried::Everywhere,
        // Everything else, named rather than left to a catch-all so that the
        // ratchet below is a statement about a set somebody wrote down:
        //   ALLOCATORS        one counter per level, for the whole store
        //   REPLICAS          who else is in the cluster is the store's
        //   RECORD_SEQUENCES  keyed by table id, tenancy not in the record
        //   VAULT_ROOT        one record, the store's
        //   VAULT_AUDIT       the store's own trail
        //   RECORD_COUNTS     derived on apply and never logged; classified anyway
        //   LEADERSHIPS       a tombstone only; the range is in the value, and
        //                     nothing in this build removes a leadership row
        _ => Carried::StoreOnly,
    })
}

/// Who a qualified-name record is carried to.
///
/// The name table is what makes `USE NAMESPACE prod` and `SELECT … FROM orders`
/// resolve, so it is the mechanism behind *cannot name another tenant's tables*
/// rather than an accessory to it: withhold `staging`'s names and the reader on
/// a `prod` follower has nothing to resolve.
///
/// The key carries the tenancy for every level that has one — that is why
/// `qualify` puts the parent ids in it — so a **tombstone** classifies as
/// exactly as well as a definition, and a dropped name reaches the follower that
/// held it. A namespace's own name has no parents, so it is read from the value
/// instead and its tombstone is unprovable.
fn named(mutation: &Mutation, value: Option<&Value>) -> Carried {
    let RecordId::Text(qualified) = &mutation.id else {
        // Every name is claimed under the qualified string itself, so anything
        // else in this table is not a name this build wrote.
        return Carried::StoreOnly;
    };
    let Some((level, parents)) = super::parse_qualified(qualified) else {
        return Carried::StoreOnly;
    };
    match (level, parents.first(), parents.get(1)) {
        (Level::Namespace, _, _) => value
            .and_then(|value| definition::id_of(value, "name", "id").ok())
            .map_or(Carried::StoreOnly, |id| {
                Carried::Schema(Reach::Namespace(NamespaceId::new(id)))
            }),
        (Level::Database, Some(&namespace), _) => {
            Carried::Schema(Reach::Namespace(NamespaceId::new(namespace)))
        }
        (
            Level::Table | Level::Index | Level::Field | Level::Graph | Level::EdgeKind,
            Some(&namespace),
            Some(&database),
        ) => Carried::Schema(Reach::Database(
            NamespaceId::new(namespace),
            DatabaseId::new(database),
        )),
        // An analyzer, a user, a replica or a consumer name: the key carries
        // no parents, because those names are unique across the store rather
        // than within a tenancy, so nothing here can prove one.
        //
        // A user's name is the case worth stating, because withholding it looks
        // like it should break sign-in on the follower and does not: `sign_in`
        // matches over the user *records*, never over this table, so a follower
        // holding its own namespace's users can sign them in with no name
        // record at all. What it cannot do is reserve a new one, which is
        // exactly what a follower has no business doing.
        _ => Carried::StoreOnly,
    }
}

/// Where a whole log record is filed.
///
/// [`carried_to`] answers for one mutation, and a record carries many: one
/// transaction's writes accumulate into a single set and are emitted as one
/// record, so `BEGIN; DEFINE ANALYZER …; CREATE person …; COMMIT` produces a
/// record holding an [`Carried::Everywhere`] mutation beside a
/// [`Carried::Within`] one. A partition over records therefore has to be total
/// over SETS of mutations, and this is that function.
///
/// The home is the [`Reach::join`] of what each mutation answers — the narrowest
/// reach that covers all of them. A record written entirely inside one database
/// homes there; one that touches two databases in a namespace homes at the
/// namespace; one that touches two namespaces homes at the store.
///
/// # Why `Everywhere` files at the store rather than everywhere
///
/// An analyzer travels to every subscriber, and the store log is the only log
/// every subscriber reads — so it is the only correct home for a record everyone
/// needs. The alternative, copying the record into each existing partition, is
/// unbounded in the number of partitions and puts one record at two positions,
/// which is the thing a log exists not to do. The cost is that a transaction
/// defining an analyzer alongside data files the whole record at the top; that is
/// accepted rather than optimised, because analyzer definitions are rare and the
/// filter on the way out still narrows what each subscriber sees.
///
/// An empty record homes at the store as well. A commit never produces one, but
/// a replica must be able to apply whatever it is sent, and the store is the
/// only answer that cannot be wrong for a record that says nothing.
///
/// # Errors
///
/// Returns [`crate::Error::CatalogMalformed`] when a mutation's definition is
/// present and cannot be decoded — the same refusal [`carried_to`] makes, and for
/// the same reason: a record whose home cannot be proven is filed nowhere rather
/// than filed wrongly.
pub(crate) fn home_of(record: &LogRecord) -> Result<Reach> {
    // A decision carries no writes to derive a home from. It belongs to the
    // coordinator's range — its record's first participant (ADR-0112 D2) — so
    // it is fenced and logged exactly as a write into that range would be. So
    // does every record that writes the transaction record: a begin and a
    // conclusion carry that range's own writes, and a conclusion whose intents
    // were all gone already carries none (D13a, D13b).
    if let Some(decided) = record.part_of().and_then(|across| across.part.record()) {
        return decided
            .participants
            .first()
            .map(|coordinator| coordinator.range)
            .ok_or(crate::error::Error::AcrossMalformed {
                part: "decide",
                problem: "a record that names no participant",
            });
    }
    if let Some(tessari_encoding::Across {
        part: tessari_encoding::Part::Forget { coordinator },
        ..
    }) = record.part_of()
    {
        return Ok(*coordinator);
    }
    // A landed part restored from a snapshot names its range (ADR-0112 D9a).
    if let Some(tessari_encoding::Across {
        part: tessari_encoding::Part::Landed { range } | tessari_encoding::Part::Prevent { range },
        ..
    }) = record.part_of()
    {
        return Ok(*range);
    }
    // A table's identity counter is advanced only by a write to that table, in
    // the same transaction, so it files wherever that write files. Counted
    // toward the home it would widen every record created with a generated
    // identity to the store — a log no database's change feed reads.
    let counted = |mutation: &tessari_encoding::Mutation| {
        mutation.namespace == system::SYSTEM_NAMESPACE
            && mutation.database == system::SYSTEM_DATABASE
            && mutation.table == system::RECORD_SEQUENCES
    };
    let rides = record.mutations().iter().any(|mutation| !counted(mutation));
    let mut home = None;
    for mutation in record.mutations() {
        if rides && counted(mutation) {
            continue;
        }
        let own = match carried_to(mutation)? {
            Carried::Within(reach) | Carried::Schema(reach) => reach,
            Carried::Everywhere | Carried::StoreOnly => Reach::Store,
        };
        home = Some(match home {
            Some(so_far) => Reach::join(so_far, own),
            None => own,
        });
    }
    Ok(home.unwrap_or(Reach::Store))
}

#[cfg(test)]
mod tests;
