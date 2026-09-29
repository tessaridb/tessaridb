//! Writing records: the sealing path, store-named identities and inserts.

use std::collections::BTreeMap;
use tessari_encoding::encode_payload;
use tessari_ql::{Answer, Name, Span, TableRef};
use tessari_storage::{Catalog, RecordAddress, Transaction};

use tessari_types::{IdentityKind, MAX_NESTING, RecordId, TableId, Value};

use crate::context::Context;
use crate::error::{Error, Result};
use crate::generate;
use crate::geometry::on_the_grid;
use crate::outcome::Outcome;
use crate::session::Session;

use super::PartialSeal;

impl Session<'_> {
    /// Write a record, after every shape in it has crossed the geometry boundary.
    ///
    /// Every record write in this crate goes through here, which is the point.
    /// The boundary is not a rule about a field that was declared `TYPE
    /// geometry` — validity is a property of the value, not of a declaration
    /// about it, and the store's default table declares nothing at all. So the
    /// check runs on whatever arrives, wherever it arrives.
    ///
    /// It cannot live in the storage layer's schema check instead. That check
    /// reads an already-encoded payload, so it has no way to snap: snapping is a
    /// transformation and the payload is downstream of it. It also returns early
    /// for a table whose schema constrains nothing, which is exactly the table
    /// most geometry will be written to.
    pub(crate) fn put_record(
        &self,
        transaction: &mut Transaction<'_>,
        address: RecordAddress,
        payload: Value,
        span: Span,
    ) -> Result<()> {
        self.put_record_sealing(transaction, address, payload, None, span)
    }

    /// The same write, made by the engine rather than by a caller.
    ///
    /// The **only** two callers are `CLAIM` and `RELEASE`, and they exist
    /// because the fields they set are exactly the fields [`Session::put_record`]
    /// refuses. A named path rather than a flag, so that "this write may set the
    /// engine's fields" is a thing the reader can see at the call site instead of
    /// a `true` in an argument list.
    pub(crate) fn put_engine_record(
        &self,
        transaction: &mut Transaction<'_>,
        address: RecordAddress,
        payload: Value,
        span: Span,
    ) -> Result<()> {
        self.write_record(transaction, address, payload, None, span)
    }

    /// The same write, told which fields a partial vault edit supplied.
    ///
    /// `None` is every caller but the two edit paths, and means "seal this
    /// record whole": mint a data key, seal every secret field, write a fresh
    /// key set. `Some` means the payload already carries the record's untouched
    /// envelopes and only the named fields are plaintext, so the record's own
    /// data key and key set are reused — which is what keeps a recipient's wrap
    /// valid across an edit.
    pub(super) fn put_record_sealing(
        &self,
        transaction: &mut Transaction<'_>,
        address: RecordAddress,
        payload: Value,
        partial: Option<&PartialSeal>,
        span: Span,
    ) -> Result<()> {
        // Every **caller-driven** record write passes through here — the two
        // creates, the insert, the update, the upsert, the set and both vault
        // edits — which is why the queue's engine-field rule sits here rather
        // than in each of them. One rule in one place, and a write path added
        // later inherits it instead of having to remember it. This placement was
        // not the first one tried: the guard sat one level up, in `put_record`,
        // and `UPDATE` reached the write without passing it.
        //
        // It takes the payload by value and hands it back because the rule has
        // two halves: refuse a caller that introduces or changes one of the
        // engine's fields, and carry forward the ones a whole-record write
        // simply left out.
        let mut payload = payload;
        crate::queue::hold_engine_fields(transaction, &address, &mut payload, span)?;
        crate::series::hold_event_time(transaction, &address, &payload, span)?;
        // A series with rollups keeps them in this same transaction (ADR-0088
        // §6); every other table pays one registry lookup for asking.
        let Some(held) = self.rollups_before(transaction, &address, span)? else {
            return self.write_record(transaction, address, payload, partial, span);
        };
        self.write_record(transaction, address.clone(), payload.clone(), partial, span)?;
        self.rollups_after(transaction, &address, held, Some(&payload), span)
    }

    /// The write itself, with no question asked about who is making it.
    ///
    /// Split from [`Session::put_record_sealing`] so that the engine's own two
    /// writes — the claim and the release, which set exactly the fields that
    /// funnel refuses — have a path that is *named* rather than a flag passed
    /// into a shared one. A reader at the call site can see which kind of write
    /// it is without following an argument.
    pub(super) fn write_record(
        &self,
        transaction: &mut Transaction<'_>,
        address: RecordAddress,
        payload: Value,
        partial: Option<&PartialSeal>,
        span: Span,
    ) -> Result<()> {
        let payload = on_the_grid(payload, span)?;
        // Sealing sits between the geometry boundary and the encoder, and the
        // order is the point: `on_the_grid` transforms values, sealing replaces
        // them with ciphertext, and nothing downstream of the encoder can tell
        // the difference — which is what closes the index, the feed, the log and
        // the backup in one move. A vault write reaching the encoder unsealed is
        // the failure this placement exists to make unreachable.
        let payload = match partial {
            Some(edit) => tessari_storage::reseal_named(
                transaction,
                &address,
                payload,
                &edit.keys,
                &edit.named,
            )?,
            None => tessari_storage::seal_secrets(transaction, &address, payload)?,
        };
        // Checked on exactly what is encoded, after sealing, because that is
        // what the decoder will be asked to follow on every read.
        if payload.nests_deeper_than(MAX_NESTING) {
            return Err(Error::NestedTooDeep {
                limit: MAX_NESTING,
                span,
            });
        }
        transaction.put(address, encode_payload(&payload).into_bytes());
        Ok(())
    }

    /// Write one record the store names itself: `CREATE users = { … }`.
    ///
    /// # Why this is the path the documentation leads with
    ///
    /// Asking a caller to invent a name per record is asking for the collision
    /// they will eventually write, and it puts a decision in front of every
    /// example that the store is better placed to make. The addressed form
    /// stays for the caller who has a name already — an import, a migration, a
    /// foreign key — which is the case it was always right for.
    ///
    /// # What it answers with, and why the identity is the default answer
    ///
    /// Without a `RETURN` clause this answers the produced identity rather than
    /// [`Outcome::Done`]. `Done` would be the one honest thing it must not say:
    /// the caller did not choose the identity, cannot derive it, and has no
    /// second statement that would find the record again — so a write that
    /// reported only that it happened would be a write nothing can reach.
    /// `RETURN AFTER` still answers the record, because a caller who asked for
    /// the record asked for the record.
    ///
    /// The shape is [`Outcome::Keys`], which is what `INSERT` already answers
    /// with for the same reason. One shape for one idea, so the surface that
    /// renders it has one arm to add rather than two to keep in step — this
    /// project's own scar on that is `plan/reported.rs`.
    pub(super) fn create_named_by_the_store(
        &self,
        transaction: &mut Transaction<'_>,
        table: &TableRef,
        value: &tessari_ql::Expr,
        answer: Answer,
        span: Span,
    ) -> Result<Outcome> {
        let (context, id) = self.resolve_table(transaction, table)?;
        // The same refusal `Session::writable` gives, reached directly for the
        // same reason `insert` reaches it directly: that one takes a record
        // target and this statement names no record.
        if Catalog::new(transaction)
            .table(id)?
            .is_some_and(|found| found.is_bucket())
        {
            return Err(Error::NotWrittenByHand {
                table: table.name.text.clone(),
                span: table.span,
            });
        }
        // Evaluated before the identity is allocated, so a value that refuses
        // does not spend a number. The counter is transactional and would roll
        // back anyway; not spending it keeps the sequence gapless for a reader,
        // and a gap in a sequence reads as a deletion.
        let payload = self.evaluate(transaction, value)?;
        let payload = self.with_defaults(transaction, id, payload)?;
        // Free by construction — `free_identity` does the read that establishes
        // it, so nothing here writes over a record that was already there.
        let identity = self.free_identity(transaction, &context, id, &payload, table.span)?;
        let address = RecordAddress::new(context.namespace, context.database, id, identity.clone());
        self.put_record(transaction, address, payload.clone(), span)?;
        Ok(match answer {
            Answer::After => Outcome::Value(payload),
            _ => Outcome::Keys(vec![identity]),
        })
    }

    /// An identity the table will give a record it is not given a name for, and
    /// which no record in it holds.
    ///
    /// # Why the scheme is the table's
    ///
    /// It is read from the declaration rather than decided here. A store that
    /// chose per statement would name records into one table under two schemes,
    /// and afterwards nothing could say which one a missing record had been
    /// written under. A table with no stored scheme reads [`IdentityKind::Int`],
    /// which is the default `DEFINE TABLE` writes and what the declaration would
    /// have said had the choice existed when that table was made.
    ///
    /// # Why the counter walks past an identity somebody named
    ///
    /// One table has **one** identity space, and the caller may write into it by
    /// hand: `CREATE users:1` and `CREATE users = { … }` address the same table.
    /// A counter that started at 1 against a table whose low identities were
    /// imported would collide, and — because a refusal discards the counter's
    /// advance along with the rest of the transaction — it would collide again
    /// on the next attempt, and every attempt after that. That is not a bad
    /// error message; it is a table that can never again be written to without
    /// naming the record. So the counter advances until it finds an identity
    /// nothing holds.
    ///
    /// **The cost is a read per identity walked past, and it is paid once.** The
    /// counter keeps its advance when the statement commits, so the skipping is
    /// amortised over the table's life rather than repeated. The shape that is
    /// genuinely slow is a table given millions of named identities from 1
    /// upwards and *then* asked to generate — one statement pays for all of
    /// them. `IDENTITY uuid` is the declaration for a table expecting that, and
    /// it needs no counter at all.
    ///
    /// A repeated **UUID** is not walked past. It cannot happen unless the
    /// machine's randomness is broken, and a store that quietly drew again would
    /// be hiding that rather than reporting it.
    pub(crate) fn free_identity(
        &self,
        transaction: &mut Transaction<'_>,
        context: &Context,
        table: TableId,
        payload: &Value,
        span: Span,
    ) -> Result<RecordId> {
        // A series ordered by event time names the record from its own time
        // field, so a late event lands in its place (ADR-0088 §1). Checked for
        // being free like any minted UUID, and for the same reason not drawn
        // again when it is not.
        if let Some(identity) =
            crate::series::event_identity(transaction, context.namespace, table, payload, span)?
        {
            let address =
                RecordAddress::new(context.namespace, context.database, table, identity.clone());
            if transaction.get(&address)?.is_some() {
                return Err(Error::RecordExists {
                    id: identity.to_string(),
                    span,
                });
            }
            return Ok(identity);
        }
        let kind = Catalog::new(transaction)
            .table(table)?
            .map_or_else(IdentityKind::default, |found| found.identity);
        loop {
            let identity = match kind {
                IdentityKind::Uuid => RecordId::Uuid(generate::uuid_v7(span)?),
                IdentityKind::Int => {
                    let number = Catalog::new(transaction).next_record_number(table)?;
                    // The counter is a `u64` and a record identity is an `i64`,
                    // so the boundary is crossed with a check rather than an
                    // `as`. It cannot refuse — the counter declines to *store* a
                    // number past `i64::MAX`, so a number it answered is one
                    // this store can spend — and the branch is written anyway
                    // because a cast that wrapped here would hand out an
                    // identity that already names a record, silently.
                    let held = i64::try_from(number).map_err(|_| {
                        tessari_storage::Error::IdSpaceExhausted {
                            level: tessari_storage::RECORD_LEVEL,
                        }
                    })?;
                    RecordId::Int(held)
                }
            };
            let address =
                RecordAddress::new(context.namespace, context.database, table, identity.clone());
            if transaction.get(&address)?.is_none() {
                return Ok(identity);
            }
            if matches!(kind, IdentityKind::Uuid) {
                return Err(Error::RecordExists {
                    id: identity.to_string(),
                    span,
                });
            }
        }
    }

    /// Write a batch of records the store names itself.
    ///
    /// # One transaction, and why that needs no code here
    ///
    /// The rows are written in a loop against the transaction this statement was
    /// handed, and a row that refuses leaves through `?` — so `session::run`
    /// never reaches its `commit`, and the rows already written go with it. The
    /// batch is atomic because the transaction is, not because anything here
    /// arranges it. The test for it uses a **middle** row, because an
    /// implementation that committed as it went would still pass a batch whose
    /// only bad row is the last.
    ///
    /// # Why the produced identity is checked against the store
    ///
    /// [`Self::free_identity`] does the read that makes the identity free, and a
    /// read per row is what that costs. A store that skipped it and wrote over a
    /// record would lose it with nothing anywhere to notice — and the case is not
    /// hypothetical, because the caller may name identities in this same table
    /// by hand.
    ///
    /// The refusal tells the caller nothing they could have used: they did not
    /// choose the identity and cannot choose the next one, so this is not the
    /// existence oracle that keeps `CREATE` at `read` + `write`. `INSERT` needs
    /// `write` alone — see `identity::Needs::of`.
    pub(super) fn insert(
        &self,
        transaction: &mut Transaction<'_>,
        table: &TableRef,
        columns: &[Name],
        rows: &[Vec<tessari_ql::Expr>],
        span: Span,
    ) -> Result<Outcome> {
        let (context, id) = self.resolve_table(transaction, table)?;
        // A bucket's records describe bytes the store holds, so one written by
        // hand can lie about them. The same refusal `Session::writable` gives,
        // for the same reason — reached here directly because that one takes a
        // record target and an insert names no record.
        if Catalog::new(transaction)
            .table(id)?
            .is_some_and(|found| found.is_bucket())
        {
            return Err(Error::NotWrittenByHand {
                table: table.name.text.clone(),
                span: table.span,
            });
        }

        let mut produced = Vec::with_capacity(rows.len());
        for row in rows {
            let mut fields = BTreeMap::new();
            for (column, value) in columns.iter().zip(row) {
                fields.insert(column.text.clone(), self.evaluate(transaction, value)?);
            }
            let payload = self.with_defaults(transaction, id, Value::Object(fields))?;

            let identity = self.free_identity(transaction, &context, id, &payload, table.span)?;
            let address =
                RecordAddress::new(context.namespace, context.database, id, identity.clone());
            self.put_record(transaction, address, payload, span)?;
            produced.push(identity);
        }
        Ok(Outcome::Keys(produced))
    }
}
