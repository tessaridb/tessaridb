//! INFO about records: versions, history, buckets and the audit trail.

use std::collections::BTreeMap;

use tessari_constants::HISTORY_EVENTS;
use tessari_encoding::NODE_ID_LEN;
use tessari_ql::{Name, RecordTarget, Span};
use tessari_storage::{Catalog, ChangeKind, Reach, Subject, Transaction};
use tessari_types::{Number, RecordId, Sequence, Value};

use crate::error::{Error, Result};
use crate::redact::seen;
use crate::session::Session;

impl Session<'_> {
    /// `INFO FOR VERSIONS OF person:1` — every surviving version of one record,
    /// the node that wrote each, and whether they are contested (G027 S4.3).
    ///
    /// # What this exists to return
    ///
    /// A record two nodes wrote without seeing each other holds two versions
    /// that neither supersedes. Every ordinary read answers with the newest of
    /// them; the other is still on disk, byte-intact, and reachable by nothing.
    /// An operator auditing for data loss finds both versions and concludes
    /// nothing was lost — the bytes are there, and what was missing until this
    /// statement is any path that returns them.
    ///
    /// # Three fields, and one of them is derived
    ///
    /// `answered` is the version an ordinary read resolves to, with the node
    /// that wrote it. `versions` is every survivor, newest first — one row on a
    /// settled record, which is most of them. `concurrent` is the flag the
    /// criterion names and it comes from `CausalVersions::is_contested`, the
    /// type that owns supersession, rather than from the length of the list: two
    /// routines answering one question come to disagree, and the disagreement
    /// here would be a contested record reported as settled.
    ///
    /// # It answers on a single-leader range too
    ///
    /// With one version and `concurrent: false`. A report that refused outside
    /// multi-master would make *is this contested?* unanswerable exactly where
    /// an operator who has just changed a namespace's class most wants to ask.
    pub(super) fn info_versions(
        &self,
        transaction: &mut Transaction<'_>,
        target: &RecordTarget,
        span: Span,
    ) -> Result<BTreeMap<String, Value>> {
        let (_, address) = self.address(transaction, target)?;
        let (surviving, concurrent) = transaction.surviving_writers(&address)?;
        let Some((newest, writer)) = surviving.first() else {
            return Err(Error::NoSuchRecord {
                id: address.id.to_string(),
                span,
            });
        };
        let described = |at: Sequence, node: &[u8; NODE_ID_LEN]| {
            Value::Object(BTreeMap::from([
                ("version".to_owned(), Value::from(at.to_string())),
                (
                    "node".to_owned(),
                    Value::from(RecordId::Uuid(*node).to_string()),
                ),
            ]))
        };
        Ok(BTreeMap::from([
            ("answered".to_owned(), described(*newest, writer)),
            (
                "versions".to_owned(),
                Value::Array(
                    surviving
                        .iter()
                        .map(|(at, node)| described(*at, node))
                        .collect(),
                ),
            ),
            ("concurrent".to_owned(), Value::Bool(concurrent)),
        ]))
    }

    /// `INFO FOR HISTORY OF orders:1` — what one record became, newest first.
    ///
    /// # It reads the log, and writes nothing
    ///
    /// The store has recorded every change since the log existed: a commit is a
    /// record carrying the address of everything it touched and what that became
    /// (`tessari_storage::feed`). So this is a projection, exactly as the change
    /// feed is — no second event store, no write on the commit path, and nothing
    /// that can disagree with what was committed.
    ///
    /// # The walk is bounded and says when it gave up
    ///
    /// Reading backwards makes a recently-written record cheap and a
    /// long-untouched one expensive, and no caller can tell which they are
    /// asking for. So the read is capped and the answer carries `complete`:
    /// `false` means older events may exist below the cap. A screen that showed
    /// the first five and implied they were all of them would be the failure
    /// this store refuses everywhere else.
    ///
    /// # It is this node's log
    ///
    /// `own_log` names the log this node writes. Today that is every record,
    /// because one writer allocates every position. When two writers allocate
    /// from independent counters there is no defined order between their
    /// sequences, so merging their logs into one timeline would present two
    /// unrelated counts as one story — the defect `tessari_storage::log`
    /// documents. A cross-writer history needs an order that does not exist yet,
    /// and inventing one here would be a console-ahead-of-the-engine answer.
    pub(super) fn info_history(
        &self,
        transaction: &mut Transaction<'_>,
        target: &RecordTarget,
        span: Span,
    ) -> Result<BTreeMap<String, Value>> {
        let (_, address) = self.address(transaction, target)?;
        let split = Catalog::new(transaction)
            .table(address.table)?
            .and_then(|table| table.shards);
        let home = Reach::Database(address.namespace, address.database);
        let subject = Subject::new(
            address.namespace,
            address.database,
            address.table,
            address.id.clone(),
        );
        // A history carries record VALUES, which `INFO FOR VERSIONS` beside it
        // never does — so it owes the field grant that every other read of a
        // value owes. `seen` is the store's one answer to "what may this session
        // read of this record", and it is public precisely so a second caller
        // cannot grow a second answer that disagrees with it.
        let visible = self.visible_in(transaction, address.table)?;
        let _ = span;
        let described = |change: &tessari_storage::Change| {
            let mut described =
                BTreeMap::from([("at".to_owned(), Value::from(change.sequence.to_string()))]);
            match &change.kind {
                ChangeKind::Written(value) => {
                    described.insert("change".to_owned(), Value::from("written"));
                    described.insert("value".to_owned(), seen(value.clone(), &visible));
                }
                ChangeKind::Removed => {
                    described.insert("change".to_owned(), Value::from("removed"));
                }
            }
            described
        };
        // A split table's record is written in its shard's log by a commit
        // touching one shard and in its database's by one touching two, so its
        // history is both, merged by the order this node committed them in
        // (G034, ADR-0084). Each event names its log, because `at` is a position
        // and a position counts only in its own log.
        if let Some(shards) = split {
            let shard = shards.shard_of(&address.id);
            let logs = [
                self.store.own_log(Reach::Shard(
                    address.namespace,
                    address.database,
                    address.table,
                    shard,
                ))?,
                self.store.own_log(home)?,
            ];
            let history = self.store.history_across(&logs, &subject, HISTORY_EVENTS)?;
            let events: Vec<Value> = history
                .events
                .iter()
                .map(|(log, change)| {
                    let mut event = described(change);
                    let named = match log.home {
                        Reach::Shard(_, _, _, shard) => format!("shard {}", shard.get()),
                        _ => "database".to_owned(),
                    };
                    event.insert("log".to_owned(), Value::from(named));
                    Value::Object(event)
                })
                .collect();
            return Ok(BTreeMap::from([
                ("events".to_owned(), Value::Array(events)),
                ("complete".to_owned(), Value::Bool(history.complete)),
                ("walked".to_owned(), Value::from(history.walked.to_string())),
            ]));
        }
        let log = self.store.own_log(home)?;
        let history = self.store.history_of(log, &subject, HISTORY_EVENTS)?;
        let events: Vec<Value> = history
            .events
            .iter()
            .map(|change| Value::Object(described(change)))
            .collect();
        Ok(BTreeMap::from([
            ("events".to_owned(), Value::Array(events)),
            ("complete".to_owned(), Value::Bool(history.complete)),
            ("walked".to_owned(), Value::from(history.walked.to_string())),
        ]))
    }

    /// `INFO FOR AUDIT` — every recorded vault read, oldest first.
    ///
    /// `BY 'ada'` narrows it to one actor, which is the shape the question is
    /// actually asked in: *this credential was compromised; what did it open,
    /// and what has to be rotated now?* Until this the answer existed only in
    /// Rust, so the operator holding that question at three in the morning had
    /// to write a program to ask it.
    ///
    /// # It takes no transaction, and that is the point
    ///
    /// The trail is written in a transaction of its own so that a `REVEAL`
    /// inside a cancelled transaction cannot roll away the record of itself.
    /// Reading it inside the caller's transaction would undo half of that: a
    /// reader would see their own uncommitted writes against the trail, and the
    /// trail is not something a caller writes to.
    ///
    /// # There is no check here, and that is not an omission
    ///
    /// The demand is declared once, where every statement's demand is declared,
    /// and it is `govern` over the store itself. A second check written here
    /// would be a second evaluator of one rule — the thing `INFO FOR ACCESS`
    /// exists as a counter-example to.
    pub(super) fn info_audit(&self, actor: Option<&Name>) -> Result<BTreeMap<String, Value>> {
        let entries = match actor {
            Some(name) => tessari_storage::reads_by(self.store, &name.text)?,
            None => tessari_storage::audit_entries(self.store)?,
        };
        Ok(BTreeMap::from([(
            "audit".to_owned(),
            Value::Array(entries),
        )]))
    }

    /// `INFO FOR BUCKET media` — the name and the ceiling, and nothing else.
    ///
    /// Short because a bucket declares little: a name and, when it should have
    /// one, the largest file it takes. What is not here is the listing — the
    /// files are records and `SELECT` answers them, so an `INFO` that also
    /// listed would be a second read path over the same rows, obeying whatever
    /// grants its own code remembered rather than the ones the reader already
    /// passes through.
    ///
    /// **It refuses a table that is not a bucket as `Unknown`**, which is the
    /// shape [`Session::info_vault`] and [`Session::info_vector`] already use.
    /// A distinct "wrong kind" answer would let a caller who may read nothing
    /// learn which names exist, and this subject is asked precisely where a
    /// caller has not been trusted with the answer yet.
    pub(super) fn info_bucket(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        span: Span,
    ) -> Result<BTreeMap<String, Value>> {
        let context = self.context(transaction, None, span)?;
        let missing = || Error::Unknown {
            entity: "bucket",
            name: name.text.clone(),
            span,
        };
        let id = Catalog::new(transaction)
            .table_id(context.namespace, context.database, &name.text)?
            .ok_or_else(missing)?;
        let definition = Catalog::new(transaction).table(id)?.ok_or_else(missing)?;
        if !definition.is_bucket() {
            return Err(missing());
        }
        Ok(BTreeMap::from([
            ("name".to_owned(), Value::from(name.text.as_str())),
            // `None` and not a zero. A ceiling of zero is a bucket nobody can
            // write to — a declaration this store refuses outright — so
            // reporting absence as zero would describe every ordinary bucket as
            // one that admits no file.
            (
                "max".to_owned(),
                definition.byte_ceiling().map_or(Value::None, |ceiling| {
                    Value::Number(Number::Integer(i64::try_from(ceiling).unwrap_or(i64::MAX)))
                }),
            ),
        ]))
    }
}
