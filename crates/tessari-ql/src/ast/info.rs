use super::*;

/// What an `INFO FOR` asks about.
///
/// Sixteen subjects, and each one has **exactly one** rule deciding what the
/// caller may see. That is why they are separate subjects rather than one with a
/// filter argument: a statement whose answer mixes two permission levels can only
/// give a partial answer or a confusing refusal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InfoSubject {
    /// `INFO FOR HISTORY OF orders:1` — what happened to one record, newest
    /// first.
    ///
    /// Distinct from [`Self::Versions`], which is a **conflict report**: it
    /// answers whether a record is contested right now, and on a single-leader
    /// range that is one version and nothing else. This answers what the record
    /// became, and when — a different question that the two were confused for
    /// until it was measured (Q-739).
    ///
    /// It reads the log rather than a second event store. The store has written
    /// one all along; what it lacked was a way to ask.
    History(RecordTarget),
    /// `INFO FOR STORE` — the namespaces.
    Store,
    /// `INFO FOR NAMESPACE` — the databases in the selected namespace.
    Namespace,
    /// `INFO FOR DATABASE` — the tables in the selected database.
    Database,
    /// `INFO FOR TABLE users` — one table's shape, fields and indexes.
    Table(TableRef),
    /// `INFO FOR GRAPH social` — the tables that belong to one graph.
    ///
    /// A graph with no members answers with an empty list rather than an error:
    /// a graph you have just declared exists, and reporting it as absent would
    /// make the first thing anyone does after declaring one look like a failure.
    Graph(Name),
    /// `INFO FOR SEARCH knowledge` — a declared search, its members and their
    /// statistics.
    Search(Name),
    /// `INFO FOR VECTOR embeddings` — one vector store's width, distance and
    /// measured recall.
    ///
    /// Distinct from `INFO FOR TABLE`, which reports fields and indexes, because
    /// the question a vector store is asked is not *what is in it* but **how good
    /// is it**: recall is the number that says whether an approximate answer is
    /// worth having, and it is the one thing the table view can never carry,
    /// since it is a property of a measurement rather than of a declaration.
    ///
    /// It reports the recall that was **measured**, and the parameters it was
    /// measured at, or says it has never been measured. It never computes a
    /// plausible figure: an approximate index whose recall came from a formula is
    /// a number nobody checked.
    Vector(Name),
    /// `INFO FOR GEO places` — one geo store's field and index.
    ///
    /// Distinct from `INFO FOR TABLE` for the reason [`InfoSubject::Vector`] is:
    /// the answer must carry the word that created the thing, or a round trip
    /// re-executes as a collection and the store stops being one.
    ///
    /// It carries no measurement, and that is not an omission. A vector index
    /// answers approximately, so what it is worth is a question only a
    /// measurement settles; a spatial index answers exactly, so there is nothing
    /// about it a number could report that the declaration does not already say.
    Geo(Name),
    /// `INFO FOR VAULT team` — one vault's fields, and which of them are sealed.
    ///
    /// Distinct from `INFO FOR TABLE` for the reason [`InfoSubject::Geo`] is:
    /// the answer must carry the word that created the thing, or a round trip
    /// re-executes as a table and the store stops being one — and here that is
    /// not a cosmetic loss, because a table has no `SECRET` to re-declare.
    ///
    /// It reports **which fields are sealed and nothing about what they hold**.
    /// That is the line this subject has to hold: an `INFO` that answered with a
    /// length, a fingerprint or a key identifier would be a slower oracle rather
    /// than none, and a reader would have no way to tell it was one.
    Vault(Name),
    /// `INFO FOR VAULT team RECORDS [AFTER team:'x'] [LIMIT n]` — the vault's
    /// record ids, a page at a time, and never a value (ADR-0092 D5).
    ///
    /// Its own subject rather than a clause on [`InfoSubject::Vault`], because
    /// it names the table for the grant check (`reach`) and the fields report
    /// names none. Ids only, because identities are keys and keys are not
    /// encrypted: this discloses nothing the key layout does not already.
    VaultRecords {
        /// The vault.
        table: TableRef,
        /// The last id of the page before.
        after: Option<Box<RecordTarget>>,
        /// How many ids at most; absent, a thousand.
        limit: Option<u64>,
    },
    /// `INFO FOR TOPIC events` — a topic's positions and its readers' (G037).
    Topic(TableRef),
    /// `INFO FOR BUCKET media` — one bucket's name and the largest file it takes.
    ///
    /// Distinct from `INFO FOR TABLE` for the reason [`InfoSubject::Vault`] is:
    /// the answer must carry the word that created the thing, or a round trip
    /// re-executes as a table and the store stops being one.
    ///
    /// **It also exists to be asked before a listing.** A route that lists a
    /// bucket needs to know a name is one, and until this subject existed there
    /// was no statement to ask — so the HTTP listing answered `200` with an
    /// empty body for a plain table while the three routes that write, read and
    /// delete a file all refused it. A caller then concluded the bucket was
    /// empty rather than absent, which is a wrong answer wearing a right one's
    /// clothes.
    Bucket(Name),
    /// `INFO FOR RECIPIENTS OF team:github` — who may one day open this record.
    ///
    /// The read half of the recipient set, and the reason the set is worth
    /// carrying at all: a set nothing can enumerate is write-only, and an
    /// application cannot answer *who can open this* by adding to it.
    ///
    /// It reports the names **and** their material, because the material is the
    /// application's own ciphertext and withholding it would make the round trip
    /// F1 asks for impossible. The store's own `#vault` entry is not among them:
    /// it is not a recipient anybody added, and listing it would invite an
    /// attempt to remove the one entry that must never go.
    Recipients(RecordTarget),
    /// `INFO FOR VERSIONS OF person:1` — every surviving version of one record,
    /// the node that wrote each, and whether they are contested.
    ///
    /// Its own subject rather than fields on an ordinary read, for the reason
    /// [`InfoSubject::Recipients`] is one: a per-record fact that almost no
    /// record has does not belong as a column on every read in the product. On a
    /// single-leader range the answer is one version and `concurrent: false`,
    /// and it answers there deliberately — a report that refused outside
    /// multi-master would make *is this contested?* unanswerable exactly where
    /// an operator who has just changed a namespace's class most wants to ask.
    Versions(RecordTarget),
    /// `INFO FOR AUDIT` — every recorded vault read; `BY 'ada'` narrows to one
    /// actor.
    ///
    /// The forensic question in the language. `REVEAL` records every read, but
    /// until this the trail could only be read from Rust — so an operator
    /// holding a compromised credential could not ask *what did it open* with a
    /// statement, which is the one moment they most need to.
    ///
    /// # The filter does not make this two subjects
    ///
    /// It is [`Option`] rather than a second variant because the whole trail and
    /// one actor's slice of it are the same answer under the same rule: the
    /// permission is identical, the shape is identical, and narrowing discloses
    /// strictly less. The rule this enum opens with — one rule per subject —
    /// is what forbids a filter that spans permission levels, and this one
    /// spans none.
    ///
    /// # It is answered only to the node's administrator
    ///
    /// The trail is stored store-wide rather than per tenancy, because a read
    /// is recorded before anyone knows whose it was. So there is no tenancy to
    /// scope the answer by, and the honest demand is `govern` held over the
    /// store itself — strictly narrower than any tenancy grant, and the reason
    /// one namespace's administrator cannot read another's reads.
    Audit(Option<Name>),
    /// `INFO FOR SEAL` — whether this process can open secrets, and until when
    /// (ADR-0092 D1).
    ///
    /// A property of the **process**, not of any vault: the master key is held
    /// per process and so is the deadline it is held to. That is why it is its
    /// own subject rather than a field on `INFO FOR VAULT`, which would make a
    /// per-vault question out of a store-wide one. It names no table, so it
    /// needs nothing beyond being signed in.
    ///
    /// `INFO FOR SEAL OF team` asks about one vault (ADR-0093 D4): its own
    /// key's state when it carries its own passphrase, the store's when it does
    /// not, and which of the two with `custody`. It names a table, so it needs
    /// what `INFO FOR VAULT` needs.
    Seal(Option<Name>),
    /// `INFO FOR USER ada` — one user's role, tenancy and grants.
    ///
    /// The one subject that refuses rather than filters, because its content
    /// *is* the permission system: a partial view of who may do what is worse
    /// than none, since it reads as the whole answer.
    User(Name),
    /// `INFO FOR USERS` — the users of the tenancy the caller administers.
    ///
    /// The sixth subject, and it exists because the fifth cannot answer the
    /// question an operator actually has: `INFO FOR USER <name>` needs a name,
    /// and a name you have forgotten was, until this, unrecoverable from the
    /// store by any route at all.
    ///
    /// It **refuses rather than filters**, exactly as [`InfoSubject::User`] and
    /// [`InfoSubject::Node`] do. That is the whole reason it is safe to add: a
    /// listing narrowed to what a `viewer` may see would be a partial account of
    /// who may do what, and a partial account reads as the whole one. So it is
    /// answered only to a caller who administers the tenancy — and then it is
    /// answered in full for that tenancy, which is a different claim from a
    /// filtered view across tenancies the caller does not hold.
    ///
    /// It carries each user's name, role and tenancy, and **not their grants**.
    /// Grants are per-user detail and stay in `INFO FOR USER <name>`, where one
    /// subject is being examined rather than counted.
    Users,
    /// `INFO FOR ACCESS TO TABLE orders` — who can reach this table, and how.
    ///
    /// The other direction of [`InfoSubject::User`]. That one answers *what may
    /// this user reach*, starting from a person; this one starts from an object
    /// and answers *who reaches it* — and an operator holding an incident needs
    /// the second question far more often than the first, because the thing they
    /// have is the table that leaked.
    ///
    /// # It is answered by asking, not by reading
    ///
    /// The answer is **not** derived from grants and authorities a second time.
    /// For every user the caller administers, the store signs a throwaway session
    /// in as that user and puts a real statement to the ordinary authorization
    /// path — the same function every `SELECT` and every `DELETE` goes through.
    /// A report that re-derived reachability would be a second evaluator, and two
    /// evaluators of one rule disagree eventually; the one that disagrees
    /// silently here is the one an auditor was trusting.
    ///
    /// Refuses rather than filters, for [`InfoSubject::User`]'s reason: its
    /// content *is* the permission system, and a partial account of who may do
    /// what reads as the whole account.
    Access(TableRef),
    /// `INFO FOR NODE` — this node's own settings, and the peers it knows.
    ///
    /// The one subject that reads **two stores**: the local `META` keyspace and
    /// the replicated catalog. It answers them as two named groups rather than
    /// one flat object, because a reader has to be able to tell which fields
    /// would follow a backup and which would not — and flattening them would
    /// make that a thing you have to remember (ADR-0020 §3).
    ///
    /// Refuses rather than filters, for `INFO FOR USER`'s reason in a different
    /// key: it names no table, so a grant check would pass over it vacuously,
    /// and roles and endpoints have no smaller truthful form to hand a viewer.
    Node,
    /// `INFO FOR KAFKA CONSUMER orders_in` — one consumer's declaration and its
    /// running state on **this** node.
    ///
    /// Two named groups rather than one flat object, for [`InfoSubject::Node`]'s
    /// reason: the declaration follows a backup and the running state does not,
    /// and flattening them would make that a thing you have to remember
    /// (ADR-0020 §3).
    ///
    /// It is also where the two refusals are reported — no exactly-once, no
    /// schema inference — because the loudest complaint about the system that
    /// has shipped this feature for years is that a consumer can be declared and
    /// not observed, and the second loudest is that its delivery guarantee is
    /// documented somewhere other than where a person configures it.
    Consumer(Name),
    /// `INFO FOR KAFKA CONSUMERS` — every declared consumer, and whether it is running.
    Consumers,
    /// `INFO FOR TOPIC CONSUMER orders_in` — one topic consumer's declaration,
    /// its running state on **this** node, and what it guarantees (ADR-0087).
    TopicConsumer(Name),
}
