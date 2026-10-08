use super::*;

impl Needs {
    /// What this statement needs.
    ///
    /// # Every statement is named, and there is no catch-all
    ///
    /// There used to be one — `_ => Self::Write` — and it read as the safe
    /// default, which is precisely why it was not. `READ` was added and fell
    /// through it, so a grant of `read` on a bucket could list the files and not
    /// open one; the mistake was invisible because the arm was doing exactly
    /// what it says. A catch-all mis-classifies a new statement *silently*, and
    /// a permission that is one class too strict looks like a bug in the grant
    /// rather than a bug here.
    ///
    /// So the match is exhaustive, the way `tables_named` and the conformance
    /// coverage list already are: adding a statement to the language will not
    /// compile until somebody says what it needs.
    pub(crate) fn of(kind: &StatementKind) -> Self {
        match kind {
            // What the drop it wraps needs (ADR-0124 D1).
            StatementKind::DropIfExists(dropped) => Self::of(dropped),
            // Reading **this node** is administering, and it is the one read
            // that is. Every other `SELECT` is governed by a grant on the table
            // it names, and `$node` names none — so left as `Read` it would be
            // checked by a loop over an empty list, which passes for reasons
            // unrelated to permission. That is the vacuous shape `tables_named`
            // already refuses `BACKUP` by name for.
            //
            // The answer is also not divisible: roles and endpoints are this
            // machine's position in a topology, and there is no smaller truthful
            // version of them to hand a `viewer` — the same reasoning that puts
            // `INFO FOR USER` here rather than beside the other four subjects.
            StatementKind::Select(select) if matches!(select.from, tessari_ql::Source::Node) => {
                Self::OPERATE_STORE
            }
            // `EXPLAIN` of the same read needs the same permission, for the
            // reason `tables_named` gives it: a plan that named a source the
            // caller may not read is a disclosure wearing a diagnostic's
            // clothes.
            StatementKind::Explain(select) if matches!(select.from, tessari_ql::Source::Node) => {
                Self::OPERATE_STORE
            }
            // A binding is only as privileged as what it holds. `LET $x =
            // (SELECT * FROM $node)` reads this node's identity through an
            // expression, and reading this node is administering — so the
            // question is asked of the expression rather than of the statement
            // word, which would have answered `Read` and handed a viewer the
            // topology.
            StatementKind::Let { value, .. }
            | StatementKind::Return { value }
            | StatementKind::Throw { value }
                if holds_node_read(value) =>
            {
                Self::OPERATE_STORE
            }
            StatementKind::Let { .. }
            | StatementKind::Return { .. }
            | StatementKind::Throw { .. }
            | StatementKind::Select(_)
            | StatementKind::Get { .. }
            // Reading a secret is reading, and demanding more here would be the
            // wrong kind of caution: it would make the grant the second lock,
            // when the key already is. A caller who may read the vault and holds
            // no key is refused at decryption; a caller who holds the key and
            // may not read the vault is refused here. Neither passes by the
            // other's route (F3).
            | StatementKind::Reveal { .. }
            | StatementKind::Keys { .. }
            // Reading a file is reading. Named rather than left to the
            // catch-all, which reads as `Write` — the default that is right for
            // every statement that changes something and wrong for this one.
            | StatementKind::Read { .. }
            // Reading a topic is reading, including the form that moves a
            // reader's stored position: that position is the reader's own place
            // in what it may read (G037).
            | StatementKind::ReadTopic { .. }
            // Settling a message is the rest of reading it (G042).
            | StatementKind::AckTopic { .. }
            | StatementKind::NackTopic { .. }
            // Explaining a read is reading: the catalog, about a table. The
            // caller must be allowed both, and `tables_named` says which.
            | StatementKind::Explain(_)
            => Self::READ,
            // `USE` and the transaction verbs change what the *next* statement
            // runs in rather than touching anything, so they demand nothing.
            //
            // They used to demand `read`, which was harmless while every role
            // began with it and is not any more: an ingestion identity holding
            // `write` alone must be able to select its database and open a
            // transaction, and demanding a read it does not hold would make the
            // model's own headline case unusable.
            //
            // **`USE` should demand *something* at the container it names**, and
            // does not yet. Without that a store-wide holder of one namespace can
            // name any other and learn whether it exists. The check needs the
            // named container resolved, which is not what the session's tenancy
            // holds at the moment `USE` runs, so it is its own slice (Q-252).
            // A scoped user is still refused by `within_tenancy`.
            // Unsealing is running the node. It is store-wide because the key
            // it unwraps is store-wide, and it is `Operate` rather than `Manage`
            // for the same reason `DEFINE NODE` is: it changes what this process
            // can do, not what the store contains.
            StatementKind::SealVault { vault: None, .. }
            | StatementKind::UnsealVault { vault: None, .. }
            | StatementKind::ChangeVaultPassphrase { vault: None, .. } => Self::OPERATE_STORE,
            // One vault carrying its own passphrase (ADR-0093 D5). Opening or
            // closing it is for whoever may `REVEAL` in it — the passphrase is
            // the second factor, and unsealing grants no read that the reader
            // did not already hold. Changing it rewrites the vault's
            // declaration, which is shaping structure, as declaring it was.
            StatementKind::SealVault { vault: Some(_), .. }
            | StatementKind::UnsealVault { vault: Some(_), .. } => Self::READ,
            StatementKind::ChangeVaultPassphrase { vault: Some(_), .. } => Self::MANAGE,
            StatementKind::Use { .. }
            | StatementKind::Begin
            | StatementKind::Commit
            | StatementKind::Cancel
            | StatementKind::Verify => Self::NOTHING,
            // Governing: deciding what somebody else may do, which is the same
            // kind of act as declaring them.
            //
            // Its own kind rather than the top of a ladder, and that is what
            // makes the owner's ninth rule expressible — an administrator can
            // declare users in a namespace without holding `read` over a single
            // record in it.
            StatementKind::DefineUser { .. }
            | StatementKind::AlterUser { .. }
            | StatementKind::DropUser { .. }
            | StatementKind::Grant { .. }
            | StatementKind::Revoke { .. } => Self::GOVERN,
            // Handing out an authority is `govern`, like every other statement
            // about who may do what — **and this class is not what bounds it**.
            // The bound is `Session::may_hand_out`, which puts the caller's own
            // held set beside the reach in the statement: you must govern there,
            // and you must already hold what you are giving away.
            //
            // It has to be there rather than here because this class cannot see
            // the reach the statement names, and that reach is the whole
            // question. Until the check existed the class was `govern` at the
            // **store** — safe, and too strict by exactly the case the model was
            // asked for, since a namespace authority could not hand out
            // authority inside their own namespace.
            StatementKind::GrantAuthority { .. } | StatementKind::RevokeAuthority { .. } => {
                Self::GOVERN
            }
            // A backup is every record in the store, past every grant and every
            // tenancy boundary. There is no permission smaller than "may see all
            // of it", so the role is the whole check — and a grant can never add
            // to it, which `within_grants` says out loud rather than leaving to
            // the fact that a backup names no table.
            //
            // **`{read, operate}` and not `operate` alone**, which is the
            // correction the ladder hid. Under a rank this was the store owner's
            // class and that identity held `read` anyway, so the coincidence was
            // invisible; decomposed, an `operate`-only identity is supposed to
            // run the cluster and see no records, and a backup is every record
            // there is.
            StatementKind::Backup { .. } => Self::READ_OPERATE_STORE,
            // A restore creates namespaces and databases, writes their records,
            // and reads a file on this node — the three kinds together, at the
            // store, which only a store-wide owner holds. Each statement it runs
            // is then checked again as the caller's own.
            StatementKind::Restore { .. } => Self::MANAGE_WRITE_OPERATE_STORE,
            // Asking about a **user** is asking what the permission system says,
            // so it is the same kind of act as writing it. The other four
            // subjects filter — they report the tables and fields the caller may
            // already read — but this one cannot: there is no smaller truthful
            // answer about who may do what, and a partial one reads as the whole
            // answer. So it refuses, and only an owner is answered.
            //
            // `INFO FOR ACCESS TO TABLE` is the same act asked from the other
            // end — who reaches this object rather than what this person
            // reaches — and the answer is made of the same material, so it sits
            // in the same class. A narrower class would be the mistake the two
            // above avoid: a listing of everyone who can read a table, handed to
            // somebody who may only read it, is a map of the permission system
            // drawn for a caller with no business in it.
            StatementKind::Info {
                subject: InfoSubject::User(_) | InfoSubject::Users | InfoSubject::Access(_),
            } => Self::GOVERN,
            // Asking about **this node** is the `$node` read wearing a
            // statement's clothes, and it lands here for exactly the reason that
            // one did: it names no table, so the grant loop passes over it
            // vacuously, and roles and endpoints are this machine's position in
            // a topology with no smaller truthful version to hand a `viewer`.
            //
            // The peer half of its answer sharpens it rather than softening it:
            // the list of every machine holding this store's data is not a
            // description of the caller's own tables.
            StatementKind::Info {
                subject: InfoSubject::Node,
            } => Self::OPERATE_STORE,
            // Configuring the node is administering it. Not `Write`, which is
            // where the other `DEFINE`s sit: an `editor` is expected to shape
            // the data they own, and neither what this machine is for nor which
            // other machines hold the data is that.
            StatementKind::DefineNode { .. }
            | StatementKind::DefineFailover { .. }
            | StatementKind::FinalizeFormat
            | StatementKind::RevokeCertificate { .. }
            | StatementKind::CreateJoinToken { .. }
            | StatementKind::DefineReplica { .. }
            | StatementKind::DropReplica { .. }
            | StatementKind::AlterReplica { .. } => Self::OPERATE_STORE,
            // Declaring a consumer is administering, not writing — the same
            // reasoning that puts `DEFINE USER` here. It hands a broker address
            // and a group name to a process that will then write into somebody's
            // table with nobody watching, which is a decision about what runs
            // rather than about what the data looks like.
            //
            // `Administer` and not `AdministerStore`, because a consumer lives in
            // the database its destination lives in: an owner of `prod.shop`
            // should be able to declare what feeds `prod.shop.orders`. The reach
            // check still applies, because `tables_named` reports the
            // destination — which is the difference between this and `BACKUP`.
            // **`{manage, write}`, which is the second correction the ladder
            // hid.** A consumer writes records on the declarer's behalf, later,
            // with nobody present. Demanding only the administrative half makes
            // it a privilege-escalation channel: declare a consumer, and records
            // appear in a table the declarer could not have written to
            // themselves. The rule generalises — *a statement that declares a
            // thing which will later act demands every authority that thing will
            // exercise* — and this is the store's only instance of it.
            StatementKind::DefineConsumer { .. } | StatementKind::DropConsumer { .. } => {
                Self::MANAGE_WRITE
            }
            // The same rule one step further: a topic consumer will also READ a
            // topic through its group, so declaring one demands that too.
            StatementKind::DefineTopicConsumer { .. } | StatementKind::DropTopicConsumer { .. } => {
                Self::MANAGE_READ_WRITE
            }
            // Asking about a consumer is asking for a broker address, a group
            // name and a running position. It **refuses rather than filters**,
            // for `INFO FOR USER`'s reason: there is no smaller truthful answer
            // about what a background writer is doing, and a partial one reads as
            // the whole one.
            //
            // This arm has to be written rather than left to the `Info` catch-all
            // below, and that is worth saying out loud: the catch-all means a new
            // `InfoSubject` does **not** raise a compile error, so the ratchet
            // that protects every other statement does not protect this one. A
            // subject added and forgotten would be answered to a `viewer`.
            StatementKind::Info {
                subject:
                    InfoSubject::Consumer(_) | InfoSubject::Consumers | InfoSubject::TopicConsumer(_),
            } => Self::MANAGE,
            // The audit trail, and it must be named here rather than left to
            // the arm below. That arm is a catch-all over `Info` alone, so a new
            // subject joins it silently — and this is the subject where that is
            // worst: read is what every signed-in caller has, and the trail is
            // every vault read in every tenancy of the store. Named, it demands
            // `govern` over the store itself.
            StatementKind::Info {
                subject: InfoSubject::Audit(_),
            } => Self::GOVERN_STORE,
            // Whether secrets can be opened right now, and until when. Named
            // rather than left to the catch-all, which demands a read: this
            // names no table, so any signed-in caller may ask (ADR-0092 D1),
            // and an anonymous one on a closed store is refused before here.
            StatementKind::Info {
                subject: InfoSubject::Seal(None),
            } => Self::NOTHING,
            // The other four are reads of the catalog, and what they report is
            // narrowed to what the caller could have found out anyway.
            StatementKind::Info { .. } => Self::READ,
            // A namespace is a **sibling** of every other namespace, and nothing
            // contains a sibling — so declaring one is not shaping the data you
            // own, it is adding to the store's top-level list. Left with the
            // `Write` block below, an `editor` of one database could do it: a
            // caller with authority over no tenancy at all, adding one.
            //
            // `DROP NAMESPACE` sits here for the same reason and not one step
            // lower: removing from the store's top-level list is the same
            // authority as adding to it, and an `editor` of one database
            // undeclaring a namespace they hold no tenancy in is exactly what
            // this level exists to refuse.
            //
            // `ALTER NAMESPACE` joins them for the same reason a third time: it
            // changes how many copies of a top-level container the cluster
            // keeps, which is a decision about the store's shape rather than
            // about anything stored in it. An `editor` of one database
            // withdrawing replication from the namespace holding it would be a
            // caller emptying a safety property they hold no tenancy over.
            StatementKind::DefineNamespace { .. }
            | StatementKind::AlterNamespace { .. }
            | StatementKind::DropNamespace { .. } => Self::MANAGE_STORE,
            // **The fifteen that were `Write`, and this line is the owner's
            // fourth rule.** Creating and dropping the containers records live
            // in is `manage`, and changing the records is `write`, and neither
            // implies the other in either direction. Under one class they were
            // the same permission: a caller allowed to write a namespace's
            // records could create and drop databases in it, and no ordering
            // over roles could have said otherwise.
            //
            // A drop keeps the same authority as the declaration it undoes
            // rather than a higher one: the person who may shape a structure is
            // the person who may unshape it, and a level that differed would
            // leave a tenant able to create what they then need somebody else to
            // remove.
            StatementKind::DefineDatabase { .. }
            | StatementKind::DropDatabase { .. }
            // A param is part of the database's own definition (ADR-0124 D2).
            | StatementKind::DefineParam { .. }
            | StatementKind::DropParam { .. }
            | StatementKind::DefineTable { .. }
            | StatementKind::DropTable { .. }
            | StatementKind::DefineSpace { .. }
            | StatementKind::DefineTopic { .. }
            // A group shapes how a topic is read, and moving one is reshaping
            // it, so all three sit with the topic's own declaration (G042).
            | StatementKind::DefineGroup { .. }
            | StatementKind::DropGroup { .. }
            | StatementKind::AlterGroup { .. }
            | StatementKind::DefineBucket { .. }
            | StatementKind::DefineCollection { .. }
            | StatementKind::DefineVector { .. }
            | StatementKind::DropVector { .. }
            | StatementKind::DefineGeo { .. }
            | StatementKind::DropGeo { .. }
            | StatementKind::DefineVault { .. }
            | StatementKind::DropVault { .. }
            // Declaring a queue is declaring a table, so it sits with the rest
            // of the structure statements. `CLAIM` and `RELEASE` are not here:
            // they write records, and they are classified below with the other
            // statements that do.
            | StatementKind::DefineQueue { .. }
            | StatementKind::DropQueue { .. }
            | StatementKind::DefineSeries { .. }
            | StatementKind::DropSeries { .. }
            | StatementKind::DefineRollup { .. }
            | StatementKind::DropRollup { .. }
            // Declaring a view is declaring a table — it takes a name in the
            // table namespace and writes a catalog entry — so it sits with the
            // structure statements even though nothing is stored under it.
            | StatementKind::DefineView { .. }
            | StatementKind::DropView { .. }
            | StatementKind::DefineGraph { .. }
            | StatementKind::DropGraph { .. }
            | StatementKind::DefineEdge { .. }
            | StatementKind::DropEdge { .. }
            | StatementKind::DefineIndex { .. }
            | StatementKind::DropIndex { .. }
            // An event is a declaration on a table, decided like an index
            // (ADR-0110 D6).
            | StatementKind::DefineEvent { .. }
            | StatementKind::DropEvent { .. }
            | StatementKind::RebuildIndex { .. }
            | StatementKind::CheckTable { .. }
            | StatementKind::AnalyzeTable { .. }
            | StatementKind::DefineField { .. }
            | StatementKind::DropField { .. }
            | StatementKind::AlterTable { .. }
            | StatementKind::AlterField { .. }
            | StatementKind::DefineAnalyzer { .. }
            | StatementKind::DropAnalyzer { .. }
            | StatementKind::DefineSearch { .. }
            | StatementKind::DropSearch { .. }
            | StatementKind::DefineSynonyms { .. }
            | StatementKind::DropSynonyms { .. }
            | StatementKind::DefineStopwords { .. }
            | StatementKind::DropStopwords { .. } => Self::MANAGE,
            // **The writes that are also reads**, and the classification is
            // measured rather than reasoned. `CREATE t:1` on an existing
            // record refuses with *record 1 already exists* and `UPDATE t:99` on
            // an absent one with *no record 99* — each is defined by a claim
            // about prior state, so each answers a question about it.
            // `DELETE … WHERE` reads records to decide which to remove, and a
            // condition that selects is a read whether or not rows come back.
            //
            // The cost is real and is stated rather than discovered: an
            // append-only writer that wants `CREATE`'s duplicate refusal must
            // also hold `read`. Letting `write` alone run it was considered and
            // refused — an oracle over record ids is exactly what turns a
            // write-only integration credential into an enumeration tool, and
            // `UPSERT` is the supported answer that needs nothing extra.
            // Only the **addressed** create is an oracle. The caller picked the
            // identity, so the refusal answers a question they asked about
            // prior state — which is what the paragraph above is about.
            StatementKind::Create {
                target: CreateTarget::Named(_),
                ..
            }
            | StatementKind::Update { .. }
            // Both recipient statements refuse on a claim about prior state —
            // *that name is already a recipient*, *that name is not one* — so
            // each answers a question about the set before it changed it, which
            // is the property this class is measured on rather than reasoned
            // about. The refusals are deliberate (a silent revocation is the
            // worst answer `REMOVE RECIPIENT` could give), and the reading half
            // is what they cost.
            | StatementKind::AddRecipient { .. }
            | StatementKind::RemoveRecipient { .. }
            | StatementKind::DeleteWhere { .. }
            // `DELETE FROM t:a..b` reads no record — it removes by position —
            // and is still in this class, because the class is measured on what
            // the answer discloses rather than on what the statement reads. It
            // answers `removed n`, and `n` is exactly how many records existed
            // in a span the **caller** chose. That is an enumeration oracle over
            // identity ranges and a binary search away from naming them, which
            // is the same argument the addressed `CREATE` above is classified
            // by.
            | StatementKind::DeleteSpan { .. }
            // A claim writes the hold **and** answers with the record, so it
            // discloses everything a `SELECT` of the same records would. The
            // class is measured on what the answer discloses, which is the same
            // argument that puts a span delete on this line.
            | StatementKind::Claim { .. }
            // The targeted form discloses the same record by the same answer, so it
            // takes the same class — named here rather than left to a catch-all,
            // which is the mistake the release's own comment below records.
            | StatementKind::ClaimRecord { .. }
            // A release clears a hold and answers nothing about the record, so
            // it is the write half alone — and it is listed here rather than as
            // a write-only statement because the class it would otherwise take
            // is decided by a catch-all arm, and a catch-all over a statement
            // family is how the next member added gets mis-permissioned.
            | StatementKind::Release { .. }
            | StatementKind::ReleaseAll { .. } => Self::READ_WRITE,
            // **The six that are measurably silent about prior state.** Every
            // one of them answers `ok` against an absent or conflicting record,
            // so a holder of `write` alone can run them and learn nothing —
            // which is what makes a pure ingestion identity a real thing here
            // rather than a theoretical one.
            StatementKind::Upsert { .. }
            // `CREATE users = { … }` is the single-record shape of the same
            // thing `INSERT` is below: the store chose the identity, so the
            // caller cannot name the one that would conflict and cannot name
            // the next one either. Its only refusal is a store whose randomness
            // or counter is broken, which tells an attacker nothing.
            | StatementKind::Create {
                target: CreateTarget::Generated(_),
                ..
            }
            // `INSERT` writes at an identity the **store** chose, so there is no
            // claim about prior state a caller could have made and no answer
            // they could read one from: they cannot name the identity that would
            // conflict, and cannot name the next one either. That is what
            // separates it from `CREATE` two arms above, whose refusal is an
            // oracle precisely because the caller picked the identity it asks
            // about. The append-only ingestion credential this arm exists for is
            // exactly the caller `INSERT` was added for.
            | StatementKind::Insert { .. }
            | StatementKind::Delete { .. }
            | StatementKind::Relate { .. }
            | StatementKind::DeleteEdge { .. }
            | StatementKind::Set { .. }
            | StatementKind::Expire { .. }
            | StatementKind::Persist { .. }
            | StatementKind::Incr { .. }
            | StatementKind::Del { .. }
            | StatementKind::Put { .. } => Self::WRITE,
        }
    }
}
