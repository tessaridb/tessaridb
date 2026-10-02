//! Whether a caller's authorities and grants reach what a statement touches.

use crate::error::{Error, Result};
use crate::identity::{At, Needs};
use crate::session::Session;
use tessari_ql::StatementKind;
use tessari_storage::{Catalog, Reach, Role, Store, Verb};

impl<'a> Session<'a> {
    /// Refuse a container this user holds no authority over.
    ///
    /// # Why this is beside the grant check and not inside the tenancy one
    ///
    /// [`Session::permits`] runs at every tenancy resolution and does not know
    /// which statement it is resolving for, so it cannot know which authority
    /// kinds are demanded — and giving it the statement would push a permission
    /// decision into the reference resolver, which is statement-agnostic on
    /// purpose. So the kinds are asked here, walking the same resolved tables
    /// [`Session::within_grants`] walks, for the same reason.
    ///
    /// # The two halves, and why neither is sufficient
    ///
    /// `Needs::unheld_by` already refused a caller who holds the demanded kind
    /// **nowhere**. That is the coarse half: it catches the common case — a
    /// holder of `write` running `DEFINE TABLE` — and it gives the refusal a
    /// message before any name is resolved. It cannot catch the other case,
    /// because holding `manage` over one database is holding it *somewhere*, and
    /// under the coarse question alone that would answer for a sibling database
    /// too.
    ///
    /// This half closes that, and it is deliberately not vacuous: a statement
    /// naming no table falls back to the tenancy the session is working in
    /// rather than to an empty loop. An empty loop reading *every container it
    /// names is held* passes for reasons that have nothing to do with
    /// permission, which is the shape this store has already had to refuse
    /// `BACKUP` by name for.
    pub(crate) fn within_authority(
        &self,
        store: &'a Store,
        kind: &StatementKind,
        span: tessari_ql::Span,
    ) -> Result<()> {
        let Some(user) = self.identity.user() else {
            return Ok(());
        };
        let needs = Needs::of(kind);
        if needs.kinds().is_empty() {
            return Ok(());
        }
        for reach in self.reaches(store, kind, needs, span)? {
            for demanded in needs.kinds() {
                if !user.authorities.permits(*demanded, reach) {
                    return Err(Error::RoleForbids {
                        role: user.role.map_or("authorities", Role::name),
                        needs: demanded.name(),
                        span,
                    });
                }
            }
        }
        Ok(())
    }

    /// The containers a statement's authority is demanded at.
    ///
    /// One entry per table it names, resolved to the database that table lives
    /// in — so a statement naming two tables in two databases is authorized
    /// twice, and a read reaching across a qualified name is asked about the
    /// database it reached rather than the one the session selected.
    ///
    /// A table that does not resolve contributes nothing: it is refused by
    /// whatever resolves it, with a message about the table rather than about an
    /// authority, and answering here first would turn *no such table* into a
    /// permission refusal that tells the reader less.
    pub(crate) fn reaches(
        &self,
        store: &'a Store,
        kind: &StatementKind,
        needs: Needs,
        span: tessari_ql::Span,
    ) -> Result<Vec<Reach>> {
        if needs.at() == At::Store {
            return Ok(vec![Reach::Store]);
        }
        let mut reaches = Vec::new();
        for table in crate::reach::tables_named(kind) {
            let mut transaction = store.begin()?;
            let resolved = self.resolve_readable_table(&mut transaction, table);
            transaction.rollback();
            if let Ok((context, _)) = resolved {
                reaches.push(Reach::Database(context.namespace, context.database));
            }
        }
        if reaches.is_empty() {
            // The fallback that stops the loop above passing vacuously. A
            // statement naming no table still acts *somewhere*, and that
            // somewhere is the tenancy the session selected — which is what
            // `DEFINE TABLE`, `DROP DATABASE` and `INFO FOR DATABASE` are all
            // asking about.
            //
            // A session that has selected nothing yet contributes no container,
            // and that is correct rather than a hole: it has resolved no name,
            // so there is nothing for an authority to be held over, and every
            // statement that goes on to name one is asked again at the naming.
            let mut transaction = store.begin()?;
            let selected = self.context(&mut transaction, None, span).ok();
            transaction.rollback();
            if let Some(context) = selected {
                reaches.push(Reach::Database(context.namespace, context.database));
            }
        }
        Ok(reaches)
    }

    /// Refuse a table this user's grants do not name.
    ///
    /// # Grants, if a user has any, are the whole story
    ///
    /// A user with none is governed by their role, which is what lets grants be
    /// added to a store whose users already work without changing what any of
    /// them may do. A user with one reaches exactly what they were granted,
    /// because a role can only widen and a permission system that cannot narrow
    /// is decoration.
    ///
    /// Which tables a statement names comes from [`crate::reach::tables_named`],
    /// an exhaustive match — so a statement form added later cannot reach a
    /// table until somebody has decided whether grants apply to it.
    pub(crate) fn within_grants(
        &self,
        store: &'a Store,
        kind: &StatementKind,
        span: tessari_ql::Span,
    ) -> Result<()> {
        let Some(user) = self.identity.user() else {
            return Ok(());
        };
        let mut transaction = store.begin()?;
        let grants = Catalog::new(&mut transaction).grants_for(user.id)?;
        transaction.rollback();
        if grants.is_empty() {
            return Ok(());
        }

        // A grant names a table that exists. Declaring one therefore has no
        // grant that could permit it, and saying so is better than a refusal
        // that reads like a bug.
        //
        // All four declarations of a table, because `reach::tables_named`
        // returns an EMPTY list for every one of them and says in its own
        // comment that the caller handles them. Anything named there and
        // missing here is not refused by the loop below either — the loop
        // iterates the tables a statement names, and these name none — so it
        // is simply allowed. `DEFINE BUCKET` was in exactly that position
        // before this line listed it.
        if matches!(
            kind,
            StatementKind::DefineTable { .. }
                | StatementKind::DefineSpace { .. }
                | StatementKind::DefineTopic { .. }
                | StatementKind::DefineBucket { .. }
                | StatementKind::DefineCollection { .. }
        ) {
            return Err(Error::GrantedUserCannotDeclare {
                user: user.name.clone(),
                span,
            });
        }

        // A backup reaches **every** table and therefore names none, so the loop
        // below — every table this statement names is granted — would pass over
        // it vacuously. That is the same shape as the defect a `READ` falling
        // through a catch-all produced, so it is refused here by name rather than
        // left to an emptiness that reads as permission.
        if matches!(
            kind,
            StatementKind::Backup { .. } | StatementKind::Restore { .. }
        ) {
            return Err(Error::GrantedUserCannotBackUp {
                user: user.name.clone(),
                span,
            });
        }

        // A grant is a verb on a table and there are two verbs, so the kinds
        // collapse here: a demand answered by reading alone asks for the read,
        // and everything else asks for the write. The statements with no table
        // to grant on are refused above by name — a granted user cannot back up
        // or declare — so this mapping is about the reach rather than about them.
        let verb = if Needs::of(kind).only_reads() {
            Verb::Read
        } else {
            Verb::Write
        };
        for table in crate::reach::tables_named(kind) {
            let mut transaction = store.begin()?;
            // A materialized view is named, not expanded, and a grant on a view
            // is never written — so its grant is its source's (ADR-0109 D7).
            let resolved = self
                .resolve_readable_table(&mut transaction, table)
                .and_then(|(context, id)| {
                    Ok(match self.materialized_view(&mut transaction, id)? {
                        Some(kept) => (context, kept.source),
                        None => (context, id),
                    })
                });
            transaction.rollback();
            // A table that does not resolve is refused by whatever resolves it,
            // with a message about the table rather than about a grant.
            let Ok((_, id)) = resolved else { continue };
            let granted = grants
                .iter()
                .any(|grant| grant.table == id && grant.verbs.contains(&verb));
            if !granted {
                return Err(Error::NotGranted {
                    user: user.name.clone(),
                    table: table.name.text.clone(),
                    needs: verb.name(),
                    span,
                });
            }
        }
        Ok(())
    }

    /// Refuse a resolved tenancy that is not the signed-in user's own.
    ///
    /// This is the check that **cannot be walked around**, and it is here rather
    /// than at `USE` for a reason: a statement may name a database directly —
    /// `SELECT * FROM other.notes` — and never touch the session's selection at
    /// all. Every path that reaches a record first resolves a namespace and a
    /// database into ids, so refusing at that resolution refuses all of them by
    /// construction. Checking only `USE` would guard the front door of a room
    /// with two.
    ///
    /// The refusal names the **tenancy** and never says whether the record or
    /// the table exists: a refusal that leaks that has answered the question it
    /// declined.
    /// `named` is the tenancy as the author wrote it, which is what the refusal
    /// echoes back. Naming it leaks nothing they did not already type, and it is
    /// the only thing here that reads as an answer to what they asked.
    pub(crate) fn permits(
        &self,
        namespace: tessari_types::NamespaceId,
        database: tessari_types::DatabaseId,
        named: &str,
        span: tessari_ql::Span,
    ) -> Result<()> {
        let Some(user) = self.identity.user() else {
            return Ok(());
        };
        let outside = user.namespace.is_some_and(|own| own != namespace)
            || user.database.is_some_and(|own| own != database);
        if outside {
            return Err(Error::OutsideTenancy {
                name: named.to_owned(),
                span,
            });
        }
        Ok(())
    }
}
