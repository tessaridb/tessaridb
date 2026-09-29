//! Granting and revoking authority, and what a grant holds.

use super::kinds::{holdable_at, kind_named};
use crate::error::{Error, Result};
use crate::info::within;
use crate::outcome::Outcome;
use crate::session::Session;
use tessari_ql::{Name, ReachRef, Span, UserGrant};
use tessari_storage::{Authority, Catalog, Held, Kind, Reach, Role, Transaction, UserDefinition};

impl Session<'_> {
    /// The reach a statement named, resolved against the catalog.
    ///
    /// The database spelling goes through `tenancy_of`, which is where the
    /// existing reach check for `DEFINE USER … ON prod.orders` lives; the
    /// namespace spelling checks the same thing one level up, because a
    /// namespace nobody may reach is not a namespace they may grant in.
    /// `pub(crate)` rather than private: `DEFINE REPLICA … REPLICATES` resolves
    /// a subscription's reach and must resolve it with the same reader a grant
    /// uses, or the two spellings of one thing acquire two meanings.
    pub(crate) fn reach_of(
        &self,
        transaction: &mut Transaction<'_>,
        named: &ReachRef,
    ) -> Result<Reach> {
        match named {
            ReachRef::Store => Ok(Reach::Store),
            ReachRef::Namespace(name) => {
                let namespace = Catalog::new(transaction)
                    .namespace_id(&name.text)?
                    .ok_or_else(|| Error::Unknown {
                        entity: "namespace",
                        name: name.text.clone(),
                        span: name.span,
                    })?;
                Ok(Reach::Namespace(namespace))
            }
            ReachRef::Database(table) => {
                let context = self.tenancy_of(transaction, table)?;
                Ok(Reach::Database(context.namespace, context.database))
            }
            // G031 — resolved by name at every level, and refused unless the
            // table is split and has that shard: a subscription to a shard that
            // does not exist is a subscription to nothing, which is the quietest
            // failure a cluster has.
            ReachRef::Shard {
                namespace,
                database,
                table,
                shard,
            } => {
                let written = format!(
                    "{}.{}.{} {shard}",
                    namespace.text, database.text, table.text
                );
                let unknown = || Error::Unknown {
                    entity: "shard",
                    name: written.clone(),
                    span: table.span,
                };
                let catalog = Catalog::new(transaction);
                let namespace = catalog.namespace_id(&namespace.text)?.ok_or_else(unknown)?;
                let database = catalog
                    .database_id(namespace, &database.text)?
                    .ok_or_else(unknown)?;
                let table_id = catalog
                    .table_id(namespace, database, &table.text)?
                    .ok_or_else(unknown)?;
                let shard = tessari_types::ShardId::new(*shard);
                let held = catalog
                    .table(table_id)?
                    .and_then(|definition| definition.shards)
                    .is_some_and(|shards| shards.holds(shard));
                if !held {
                    return Err(unknown());
                }
                Ok(Reach::Shard(namespace, database, table_id, shard))
            }
        }
    }

    /// The set a `DEFINE USER` declares, however it spelled it.
    ///
    /// A role is a name for a set, so both spellings arrive here as one — which
    /// is what stops the two from meaning different things in different places.
    pub(crate) fn held_from(&self, grant: &UserGrant, reach: Reach, span: Span) -> Result<Held> {
        match grant {
            UserGrant::Role(named) => {
                let Some(role) = Role::parse(&named.text) else {
                    return Err(Error::NoSuchRole {
                        name: named.text.clone(),
                        span,
                    });
                };
                Ok(Held::from_role(role, reach))
            }
            UserGrant::Authorities(kinds) => {
                let mut held = Held::nothing();
                for named in kinds {
                    let kind = kind_named(named)?;
                    holdable_at(kind, reach, span)?;
                    held.add(Authority::new(kind, reach));
                }
                Ok(held)
            }
        }
    }

    /// `GRANT manage ON NAMESPACE prod TO ada` — add to what a user holds.
    ///
    /// Adding rather than replacing, because a user reaches more than one place
    /// and a grant names one of them: replacing would make every grant a silent
    /// revocation of every other.
    pub(crate) fn grant_authority(
        &self,
        transaction: &mut Transaction<'_>,
        kinds: &[Name],
        reach: &ReachRef,
        user: &Name,
        span: Span,
    ) -> Result<Outcome> {
        // Kinds first: what the statement says is wrong with *itself* is
        // answered before anything about the store is looked up, so a mistyped
        // kind reads as a mistyped kind rather than as an unknown user.
        let kinds = kinds.iter().map(kind_named).collect::<Result<Vec<_>>>()?;
        let reach = self.reach_of(transaction, reach)?;
        // Before `may_hand_out`, and the order is the message. Asked after it, a
        // namespace owner naming `replicate` would be told they do not hold it —
        // which is true, and sends them to ask somebody for a grant that nobody
        // can make. Asked here, they are told the kind does not come in that
        // size.
        for kind in &kinds {
            holdable_at(*kind, reach, span)?;
        }
        self.may_hand_out(&kinds, reach, span)?;
        let mut found = self.user_to_change(transaction, user, span)?;
        // And the reach has to meet the subject's own `ON` somewhere, or the
        // grant would be stored and never usable: a declared tenancy is a second
        // confinement asked before the held set, so `GRANT read ON NAMESPACE
        // staging TO nina` — nina being of `prod` — used to return `ok` and do
        // nothing at all. Refused rather than accepted-and-inert because the
        // operator's only evidence that a grant landed is the statement not
        // complaining, and because the escalation `WiderThanYou` refuses at
        // declaration would otherwise simply move here.
        //
        // **Overlap in either direction, not containment in one.** A reach
        // inside her tenancy is usable there, and so is a reach that *contains*
        // it — `read ON STORE` granted to a user of `prod` is not inert, because
        // containment runs downward and `prod` is inside the store. Only two
        // tenancies that miss each other entirely produce a holding nothing can
        // ever consult.
        let (namespace, database) = reach.parts();
        let overlaps = within(found.namespace, found.database, namespace, database)
            || within(namespace, database, found.namespace, found.database);
        if !overlaps {
            return Err(Error::OutsideTheirTenancy {
                user: user.text.clone(),
                span,
            });
        }
        for kind in kinds {
            found.authorities.add(Authority::new(kind, reach));
        }
        self.rewrite_authorities(transaction, found);
        Ok(Outcome::Done)
    }

    /// Refuse a grant that would reach past the caller's own holdings.
    ///
    /// # The one statement that can escalate, and the two questions that stop it
    ///
    /// Every other statement is bounded by what the caller may do *now*. A grant
    /// is bounded by what somebody may do *later*, which is why it is the only
    /// place in the store where a mistake compounds: mint an identity above your
    /// own and every other check becomes decorative, because the way past them
    /// all is to be somebody else.
    ///
    /// So two questions, and they are the same rule read from both ends:
    ///
    /// 1. **Do you govern there?** Handing out authority in a namespace is an
    ///    act *in* that namespace, and `govern` is the kind that covers it.
    /// 2. **Do you hold what you are handing out?** Containment does the work:
    ///    a holder of `manage` over `prod` passes for `prod.shop` and fails for
    ///    the store, because [`Reach::contains`] runs downward and only downward.
    ///
    /// Until this existed the statement demanded `govern` at the **store**,
    /// which was safe and too strict by exactly the case the model was asked
    /// for: a namespace authority could not hand out authority inside their own
    /// namespace. The under-grant was deliberate and is now paid off — an
    /// under-grant is a support request, and an over-grant is unrecoverable.
    ///
    /// Anonymous callers are unbounded here, and that is the same rule that lets
    /// an empty store declare its first user: a store with nobody in it hides
    /// nothing from anybody, and a closed one refuses long before this.
    pub(crate) fn may_hand_out(&self, kinds: &[Kind], reach: Reach, span: Span) -> Result<()> {
        let Some(user) = self.identity.user() else {
            return Ok(());
        };
        if !user.authorities.permits(Kind::Govern, reach) {
            return Err(Error::CannotHandOut {
                kind: Kind::Govern.name(),
                span,
            });
        }
        for kind in kinds {
            if !user.authorities.permits(*kind, reach) {
                return Err(Error::CannotHandOut {
                    kind: kind.name(),
                    span,
                });
            }
        }
        Ok(())
    }

    /// `REVOKE manage ON NAMESPACE prod FROM ada` — take exactly what is named.
    ///
    /// Removing an authority the user does not hold is not an error: the
    /// statement asks for a user without it, and a user without it is what it
    /// leaves. That is `DROP USER`'s rule and it is the same rule, because a
    /// revocation that fails halfway through a list is worse than one that is
    /// idempotent.
    pub(crate) fn revoke_authority(
        &self,
        transaction: &mut Transaction<'_>,
        kinds: &[Name],
        reach: &ReachRef,
        user: &Name,
        span: Span,
    ) -> Result<Outcome> {
        let kinds = kinds.iter().map(kind_named).collect::<Result<Vec<_>>>()?;
        let reach = self.reach_of(transaction, reach)?;
        // Only the first of the two questions. Taking an authority away can
        // never mint one, so holding what is being revoked is not required —
        // and requiring it would mean a namespace administrator could not clean
        // up a grant somebody above them made.
        self.may_hand_out(&[], reach, span)?;
        let mut found = self.user_to_change(transaction, user, span)?;
        for kind in kinds {
            found.authorities.remove(&Authority::new(kind, reach));
        }
        self.rewrite_authorities(transaction, found);
        Ok(Outcome::Done)
    }

    /// Write a changed authority set back, with the role summary re-derived.
    ///
    /// Re-derived rather than carried, for `create_user`'s reason: a stored role
    /// left behind by a revocation would describe authorities the user no longer
    /// holds, and it is the field an older binary believes.
    /// A tenancy that is not a place summarises as no role at all, rather than
    /// leaving the old one behind. Nothing reachable through the language builds
    /// that record — `from_value` refuses it — but `UserDefinition` is a public
    /// struct with public fields, so the shape is constructible by a caller of
    /// this crate, and the two ways to be wrong here are not symmetric: a stale
    /// role describes authorities the user no longer holds, and it is the field
    /// an older binary believes.
    pub(crate) fn rewrite_authorities(
        &self,
        transaction: &mut Transaction<'_>,
        mut user: UserDefinition,
    ) {
        user.role = Reach::of(user.namespace, user.database)
            .and_then(|reach| user.authorities.role_within(reach));
        Catalog::new(transaction).update_user(&user);
    }
}
