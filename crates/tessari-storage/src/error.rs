//! Failures raised by the record store and its transactions.
//!
//! The categories are the substrate's, not a second vocabulary: a caller
//! already branches on [`ErrorCategory`], and two parallel taxonomies for the
//! same question is how one of them ends up unhandled.
//!
//! `Conflict` deserves a note. It is **not** a transient fault and must not be
//! retried blindly. It is a semantic outcome: another transaction committed to
//! a record this one wrote, so this one's decisions were made against a state
//! that no longer holds. The caller re-reads and decides again — which may well
//! be to do nothing.

use tessari_kv::ErrorCategory;
use tessari_types::{Epoch, FieldKind, RecordId, Sequence, article};

/// Result alias for every fallible operation in this crate.
pub type Result<T> = std::result::Result<T, Error>;

/// A failure from the record store.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// A sealed value, a key or the keyring refused.
    ///
    /// Carried through rather than flattened, because the distinctions the
    /// vault draws are the ones an operator needs: sealed is not the same
    /// answer as wrong key, and neither is the same as a format this build does
    /// not know. None of them names a value.
    #[error("{0}")]
    Vault(#[from] tessari_vault::Error),

    /// The keyring cannot be read because a thread panicked while holding it.
    ///
    /// Reported rather than recovered from. The safe answer to "can you open
    /// secrets" when this process cannot say what it holds is no.
    #[error("the keyring is unavailable in this process")]
    VaultUnavailable,

    /// A write to a vault carried the entry that holds its wrapped keys.
    ///
    /// The name is not one the grammar produces as a bare identifier, but a
    /// quoted field name accepts any text, so the collision is refused here
    /// rather than assumed impossible. Accepting it would let a caller supply
    /// its own key set and choose which key a later read opens with.
    #[error("`{field}` is reserved: a vault holds its wrapped keys under that name")]
    VaultReservedField {
        /// The reserved name.
        field: &'static str,
    },

    /// A vault was written a record that is not an object.
    ///
    /// A vault's records have fields, because a secret is a field and the
    /// wrapped key set is a field beside it. There is nowhere to put either in
    /// a bare value.
    #[error("a record in vault `{table}` must be an object")]
    VaultNotAnObject {
        /// The vault's name.
        table: String,
    },

    /// A read of a vault could not be recorded, so it is refused.
    ///
    /// The refusal is the mechanism and not a side effect. A store that serves
    /// when it cannot record is a store whose audit trail an attacker disables
    /// first, after which their reads leave no trace while every dashboard
    /// reports health. The cost — a broken trail is an outage of every read —
    /// is real, and is engineered around with a second device rather than by
    /// making the trail best-effort.
    #[error("this read cannot be recorded and so is refused: {reason}")]
    AuditUnavailable {
        /// Why the trail could not be written.
        reason: String,
    },

    /// A caller named the store's own entry as a recipient.
    ///
    /// `#vault` is the entry the engine wraps the record's data key into, and it
    /// is the one name in the set that means something here. Letting a caller
    /// write it would let them choose what a later read opens with; letting them
    /// remove it would crypto-shred the record while reporting success.
    #[error("`{recipient}` is the vault's own entry and is not a recipient")]
    VaultReservedRecipient {
        /// The reserved name that was named.
        recipient: String,
    },

    /// A recipient of that name is already on the record.
    ///
    /// Refused rather than replaced. Overwriting would destroy the only copy of
    /// whatever the existing entry held, silently and in one statement; a caller
    /// who means to replace says so in two.
    #[error("`{recipient}` is already a recipient of this record")]
    VaultRecipientExists {
        /// The name that was already there.
        recipient: String,
    },

    /// No recipient of that name is on the record.
    ///
    /// The important refusal of the pair. A revocation that matched nothing and
    /// answered `ok` would leave the operator believing a party was removed
    /// while their entry is still on the record — the one failure mode where
    /// silence is worse than an error by a wide margin.
    #[error("`{recipient}` is not a recipient of this record")]
    VaultNoRecipient {
        /// The name that was not there.
        recipient: String,
    },

    /// A key that must be there is not.
    ///
    /// Two shapes reach here and both are structural rather than cryptographic:
    /// a table declared a vault with no wrapped key on its declaration, which
    /// the catalog refuses to build in the first place, and a record with no
    /// wrapped key set — what a record written before its table became a vault
    /// looks like. Neither names a value, and neither says whether a passphrase
    /// was right.
    #[error("no key for vault `{table}`: this record cannot be opened")]
    VaultNoKey {
        /// The vault's name.
        table: String,
    },

    /// Another transaction committed to a record this one wrote.
    ///
    /// Under snapshot isolation the first committer wins. Nothing was written.
    #[error(
        "write conflict on record {id}: it was committed at sequence {committed} \
         after this transaction's snapshot {snapshot}"
    )]
    Conflict {
        /// The record that was written by both transactions.
        id: RecordId,
        /// The snapshot this transaction read at.
        snapshot: Sequence,
        /// The sequence the winning transaction committed at.
        committed: Sequence,
    },

    /// The commit could not claim a sequence within its attempt budget.
    ///
    /// Every attempt lost the race for the committed tail. This is contention,
    /// not a defect, and it is reported rather than retried forever.
    #[error("commit gave up after {attempts} attempts: the committed tail moved every time")]
    CommitContention {
        /// How many attempts were made.
        attempts: u32,
    },

    /// The lease this node writes under has run out.
    ///
    /// Not a defect and not a conflict: this node was the leader and can no
    /// longer prove it still is, so it stops writing rather than accepting work
    /// the next leader will never see. The duration is how long the fence has
    /// been closed, because a caller one second past it and a caller an hour
    /// past it are in very different situations and the bare refusal spells
    /// them the same way.
    #[error(
        "the lease this node writes under ran out {for_the_last:?} ago: \
         it is no longer accepting writes"
    )]
    LeaseSpent {
        /// How long the fence has been closed.
        for_the_last: std::time::Duration,
    },

    /// A stated failover policy named a period under a second.
    ///
    /// The four refusals below exist because the relations between these
    /// periods hold today only because the compiler holds them — three of the
    /// values default to expressions over the others. Handed to an operator,
    /// each relation breaks **silently**, which is why breaking one is an error
    /// rather than a warning.
    #[error(
        "a failover policy's `{field}` is {stated:?}, and every period in one \
         must be at least a second: a cadence that never waits is a spin, and a \
         lease of zero is spent at the instant it is taken"
    )]
    FailoverPeriodTooShort {
        /// Which period was named.
        field: &'static str,
        /// What it was set to.
        stated: std::time::Duration,
    },

    /// A stated campaign cadence is slower than the window it has to act inside.
    ///
    /// The **too large** direction, and the one that reports nothing: a leader
    /// steps over the moment it was supposed to stand, loses a lease it could
    /// have renewed, and no round was ever attempted so nothing failed.
    #[error(
        "a failover policy's `campaign` is {campaign:?} against a `round` of \
         {round:?}, and it may not exceed {ceiling:?} — twice the round time. \
         The window between the moment a leader should stand and the moment its \
         fence shuts is exactly two round times, so a slower cadence steps over \
         it: the leader loses a leadership it could have kept, and nothing \
         reports a failure because no round was ever attempted"
    )]
    FailoverCampaignOutpaced {
        /// The cadence that was stated.
        campaign: std::time::Duration,
        /// The round time it is judged against.
        round: std::time::Duration,
        /// The largest cadence that round time admits.
        ceiling: std::time::Duration,
    },

    /// A stated lease leaves no instant at which a holder may write and is not
    /// already campaigning.
    ///
    /// The **too small** direction. A holder's usable window is the lease less
    /// the fence guard, and standing opens two round times before that window
    /// ends; at or below the floor the two meet and the window is empty.
    #[error(
        "a failover policy's `lease` is {lease:?}, and it must exceed {floor:?} \
         — the {guard:?} fence guard plus two {round:?} round times. A holder's \
         usable window is the lease less the guard, and standing opens two round \
         times before that window ends, so at or below this there is no instant \
         at which a holder is both writable and not yet campaigning"
    )]
    FailoverLeaseTooShort {
        /// The lease that was stated.
        lease: std::time::Duration,
        /// The fence guard, which is not settable.
        guard: std::time::Duration,
        /// The round time the margin is built from.
        round: std::time::Duration,
        /// The shortest lease those two admit.
        floor: std::time::Duration,
    },

    /// A stated collection period is at or above the staleness floor the same
    /// policy publishes.
    ///
    /// The **too large** direction, and it fails while everything is working: a
    /// follower that collects no more often than the tightest bound the API
    /// admits is advertising a promise it cannot keep on a healthy network.
    #[error(
        "a failover policy's `collection` is {collection:?}, and it must be \
         under {floor:?} — twice the {awareness:?} awareness period, which is \
         the tightest staleness bound this node admits. A collection period at \
         or above that floor advertises a bound the node cannot meet even when \
         everything is working"
    )]
    FailoverCollectionAboveFloor {
        /// The collection period that was stated.
        collection: std::time::Duration,
        /// The awareness interval the floor is derived from.
        awareness: std::time::Duration,
        /// The floor those derive.
        floor: std::time::Duration,
    },

    /// This node is in a cluster and has not been given a leadership yet.
    ///
    /// A different state from [`Self::LeaseSpent`] and deliberately a different
    /// refusal. *Your lease ran out* names a leadership this node held and lost,
    /// and sends an operator to look at why it could not renew. *No leadership
    /// yet* names one it has never had — a cluster that has not elected anybody,
    /// or a node that has not yet won a round — and sends them somewhere else
    /// entirely. Spelling both as the lapse would report a fence closing on a
    /// node that was never behind one.
    ///
    /// # The sentence used to say *takes part in deciding*, and that became
    /// false
    ///
    /// ADR-0069 moved the condition from the `coordinating` role to the
    /// catalog, so this now reaches a node that was never declared to take part
    /// in anything and simply has a peer. Such a node cannot win a round
    /// either — nothing campaigns unless the role says to — so the message
    /// names the remedy rather than only the state.
    #[error(
        "this node is in a cluster and holds no leadership: \
         it does not accept writes until a majority grants it one. \
         A node that should be leading needs the coordinating role \
         (DEFINE NODE ROLES ... coordinating) so that it stands for one"
    )]
    NoLeadershipYet,

    /// A write belongs to a range another node leads, and this is which one.
    ///
    /// The other half of the admission question, and the half ADR-0069 could
    /// not ask. [`Self::NoLeadershipYet`] answers *nobody here holds one*; this
    /// answers *somebody else holds this one*, and they are different sentences
    /// to a caller: the first says wait, the second says go there.
    ///
    /// # Why this became possible to ask
    ///
    /// A lease is store-wide and a leadership is per-range, so *may this node
    /// write* and *may this node write here* stopped being one question the
    /// moment two nodes could lead two namespaces. A node holding a leadership
    /// over one namespace and writing into another meets this while holding a
    /// perfectly live lease — the arrangement G025's S6.1 exists to make
    /// representable, and the one the store-wide gate accepted silently.
    ///
    /// # The three fields are [`tessari_session::Peer`]'s, deliberately
    ///
    /// This engine already has one spelling of *a copy you do not hold and
    /// where to find it*, and a second one that carried different fields would
    /// be two answers to one question. The node id is what makes the redirect
    /// checkable on arrival: a client that dialled the address and met a
    /// different node would otherwise have no way to notice. The epoch dates
    /// the claim, so a client already told about a newer leadership can refuse
    /// this one rather than follow it backwards.
    ///
    /// The range is not carried. The caller issued the write and knows what it
    /// addressed; a field restating it would be a second source for a fact the
    /// statement already holds.
    #[error(
        "this range is led by another node: write it at {endpoint}. \
         This node refuses rather than forwarding on your behalf. \
         The node to expect is {}, leading under epoch {epoch}",
        tessari_types::RecordId::Uuid(*node)
    )]
    WriteIsElsewhere {
        /// The address to dial — the same string the membership row carried.
        endpoint: String,
        /// Who the log says leads it, so the redirect is checkable on arrival.
        node: [u8; tessari_encoding::NODE_ID_LEN],
        /// The leadership that node took the range under.
        epoch: Epoch,
    },

    /// A write met a record whose stored versions disagree with each other.
    ///
    /// Two nodes wrote this record without either having seen the other's
    /// write, and both versions survive because neither supersedes the other.
    /// A third write cannot be taken: the writer holds one of them, and
    /// committing on top would discard the other with nothing recording that it
    /// had ever existed. **That silent discard is the whole of what this engine
    /// refuses** (ADR-0075) — the alternative is not "resolving" the conflict,
    /// it is picking a winner and calling it an answer.
    ///
    /// Retryable in the only sense that matters: the caller reads the record,
    /// sees both versions, decides which the data means, and writes the decision
    /// having seen both — at which point its stamp descends them and this
    /// refusal does not fire. Retrying the same write unchanged meets it again,
    /// correctly.
    ///
    /// Named rather than counted. A refusal that said only *contested* would
    /// leave the caller unable to fetch what it has to choose between, which is
    /// the same dead end a redirect with no address gives.
    #[error(
        "the record {id} holds versions {ours} and {theirs} that neither \
         supersedes: they were written without seeing each other. \
         Version {theirs} carries a write from node {}, which version {ours} \
         has not seen. Read both and write what they mean.",
        tessari_types::RecordId::Uuid(*node)
    )]
    ConcurrentVersions {
        /// The record. The caller addressed it and this names which of the
        /// addresses in a multi-record commit was the one that stopped it.
        id: RecordId,
        /// The surviving version this store read first — the newest of them.
        ours: Sequence,
        /// The surviving version it is concurrent with.
        theirs: Sequence,
        /// A node whose write `theirs` carries and `ours` does not. The first in
        /// node order when there is more than one, and there is exactly one for
        /// every two-master case this engine can currently produce.
        node: [u8; tessari_encoding::NODE_ID_LEN],
    },

    /// A log record was offered out of order.
    ///
    /// State is a deterministic function of the log, so a gap is not something
    /// to skip past: applying the record anyway would leave a state that no log
    /// explains, and nothing downstream could ever detect that it had.
    #[error("log gap: the next record must be {expected}, but {found} was offered")]
    LogGap {
        /// The sequence the store is ready to apply.
        expected: Sequence,
        /// The sequence that was offered instead.
        found: Sequence,
    },

    /// Two leaderships wrote the same log position.
    ///
    /// The already-applied branch exists for an ordinary retry, and for a store
    /// with one writer a retry is the only thing that can arrive at a position
    /// it already holds. A cluster makes a second writer possible for the window
    /// between a leader failing and being noticed, and the records those two
    /// wrote collide. Accepting the offered one silently keeps whichever data
    /// this node happened to have, with no error and no gap, so two nodes answer
    /// differently while both report healthy.
    ///
    /// Not retryable: the same record will still be from the other branch. The
    /// node re-bootstraps (ADR-0059).
    ///
    /// Named for what the databases call it. Kafka added a leader epoch to the
    /// log for exactly this failure (KIP-101) after the high-watermark protocol
    /// was found to diverge logs silently; PostgreSQL increments a timeline on
    /// promotion; MongoDB carries a term in each oplog entry and rolls back to
    /// the common point. It is not a chain's fork and the epoch is not a block
    /// height.
    #[error(
        "log divergence at {sequence}: this store holds epoch {held}, \
         and epoch {offered} was offered"
    )]
    LogDivergence {
        /// The position both leaderships wrote.
        sequence: Sequence,
        /// The leadership whose record this store already applied.
        held: Epoch,
        /// The leadership whose record was offered instead.
        offered: Epoch,
    },

    /// A catalog name is already in use at that level.
    ///
    /// Raised by the read that precedes the write. It is a courtesy, not the
    /// enforcement: uniqueness is enforced by both transactions writing the same
    /// name record, so a creation that races past this check still loses at
    /// commit with [`Error::Conflict`].
    #[error("the name {qualified} is already in use")]
    NameTaken {
        /// The qualified name, including its level tag and parent ids.
        qualified: String,
    },

    /// An index was declared over no fields.
    ///
    /// Refused rather than stored, because an index keyed by nothing is not a
    /// degenerate index — it is one entry for the whole table, and a unique one
    /// would admit a single record and refuse every other with a conflict that
    /// names no field.
    #[error("index {name} declares no fields")]
    EmptyIndex {
        /// The name the index was to be created under.
        name: String,
    },

    /// A unique index already holds this value for a different record.
    ///
    /// Not a write conflict: no concurrent transaction is involved, and retrying
    /// the same write cannot succeed. The caller's data violates a constraint it
    /// declared.
    #[error("unique index {index} already holds that value; record {id} was refused")]
    UniqueViolation {
        /// The index that refused the write.
        index: String,
        /// The record that was being written.
        id: RecordId,
    },

    /// A field holds a value of a type its table does not declare for it.
    ///
    /// Like [`UniqueViolation`](Self::UniqueViolation) this is the caller's data
    /// against a constraint the caller declared, not a race — retrying the same
    /// write cannot succeed. The message names all three of the field, what was
    /// declared and what was found, because a message carrying only the first is
    /// a message the reader has to go and look two things up to act on.
    #[error(
        "table {table} declares {field} as {declared}, but record {record} holds {found} there"
    )]
    SchemaViolation {
        /// The table whose declaration was violated, by the name a declaration
        /// uses.
        ///
        /// Boxed for the reason [`found`](Self::SchemaViolation::found) is, and
        /// adding it is what made the rest of this variant boxed too: this is
        /// the widest refusal there is, four owned names carrying a `String`'s
        /// spare capacity each put it past `clippy::result_large_err`, and the
        /// lint is measured against the whole `Result` every call in three
        /// crates returns.
        table: Box<str>,
        /// The record that was being written.
        record: Box<str>,
        /// The field that disagreed.
        field: Box<str>,
        /// The type the table declares for it.
        /// Owned rather than `&'static str`: a literal union spells itself as
        /// its members, so not every declared type is a word this binary knows
        /// at compile time.
        declared: Box<str>,
        /// The type the record held instead.
        ///
        /// Owned for the same reason `declared` is, and it became necessary for
        /// the same reason: against a parameterised declaration a bare type name
        /// is true and useless. "declares embedding as vector<768>, but record
        /// documents:1 holds array there" tells a reader nothing they did not
        /// write themselves, because the value **is** an array — the width is
        /// the whole disagreement, so the width has to be in the message.
        ///
        /// A `Box<str>` and not a `String`, which is not a micro-optimisation.
        /// This enum travels in every `Result` the store returns, and the third
        /// word a `String` carries — the spare capacity a message never grows
        /// into — pushed the `Err` variant past the width
        /// `clippy::result_large_err` allows. Boxed, it is exactly the size the
        /// `&'static str` here used to be.
        found: Box<str>,
    },

    /// A value its field's declaration refuses.
    ///
    /// Checked on the apply path beside the type check, and for the same reason:
    /// the verdict is a pure function of the record and the catalog, so every
    /// replica reaches it without anything being sent.
    ///
    /// The message names the other field when the declaration compares against
    /// one, because "`ends_at` is refused" without "compared with `starts_at`"
    /// leaves the writer to guess which of the record's fields the constraint
    /// was about. Both fields came from the statement they just sent, so naming
    /// the second discloses nothing they did not supply — which is the line this
    /// message stays on: it never names a value, and never names a second
    /// **record**.
    #[error("record {record} of table {table} holds a {field} its declaration refuses{}",
        .compared_with.as_ref().map_or_else(String::new, |other| format!(" (it is compared with {other})")))]
    AssertionViolation {
        /// The table whose declaration was violated, by the name a declaration
        /// uses.
        table: Box<str>,
        /// The record that was being written.
        record: String,
        /// The field that disagreed.
        field: String,
        /// The other fields of the same record the declaration compares against,
        /// when it names any.
        compared_with: Option<String>,
    },

    /// A required field that holds nothing.
    ///
    /// "Required" covers both absence and `null`, deliberately: a field that
    /// must be present but may hold nothing is a constraint that constrains
    /// almost nothing, and the distinction between the two stays available on
    /// every field that is not required.
    #[error("record {record} in table {table} leaves required field {field} holding {found}")]
    MissingRequiredField {
        /// The table the record is in, by the name a declaration uses.
        table: Box<str>,
        /// The record's identity.
        record: String,
        /// The field that must hold a value.
        field: String,
        /// What it holds instead.
        found: &'static str,
    },

    /// A record carries a field a `SCHEMAFULL` table does not declare.
    ///
    /// This is the misspelling that a schemaless table accepts in silence: the
    /// record lands, nothing is raised, and every query filtering on the name
    /// that was meant is quietly missing it.
    ///
    /// # Why every refusal here names its table the way a declaration would
    ///
    /// The fix for one of these is a declaration, and a declaration names its
    /// table — so an internal id is a number the reader cannot write anywhere.
    /// The name costs nothing to obtain: the schema is built from the table's
    /// definition, which holds it.
    ///
    /// This variant carried the name first and its siblings carried an id, and
    /// the reason recorded here for that was *"the refusals around it identify
    /// a table by id, which is what this layer has"*. That was wrong on both
    /// halves. The layer has the name — `check` holds the `TableSchema` that
    /// carries it, and read it two match arms above the arm that wrote the id.
    /// And "the refusal a caller acts on" does not separate one of these from
    /// the others: every refusal in this group is the caller's data against a
    /// constraint the caller declared, none of them can succeed on retry, and
    /// each is fixed by changing the declaration or the record. Kept as a note
    /// rather than deleted, because the id survived in three variants for as
    /// long as this paragraph explained it.
    #[error("table {table} declares no field {field}, and record {record} carries one")]
    UndeclaredField {
        /// The table that refused the write, by the name a declaration uses.
        table: String,
        /// The record that was being written.
        record: String,
        /// The field it carried.
        field: String,
        /// The kind a declaration would have to give that field to accept it.
        ///
        /// Derived from the value the caller just sent, so it names nothing
        /// about the table's other declarations — a field a caller's grants
        /// hide is never mentioned by a refusal (ADR-0044). It is carried
        /// rather than rendered because writing it as a statement needs the
        /// language, and this layer deliberately does not have it.
        ///
        /// Boxed because every `Result` in this crate and the two above it
        /// reserves room for the widest refusal there is, and this variant now
        /// holds three names; unboxed it took that budget past what
        /// `clippy::result_large_err` allows, which is a real cost paid on
        /// every call that never fails.
        kind: Box<FieldKind>,
    },

    /// Several records in one commit were refused, and here is each of them.
    ///
    /// A commit is all-or-nothing, so the first bad record already decides the
    /// outcome and reporting only that one is *correct*. It is also the shape
    /// that makes a caller fix a batch one round trip per mistake, discovering
    /// the second problem only after the first is gone. So every record is
    /// checked before any refusal is raised.
    ///
    /// Exactly one refusal is never wrapped: a batch of one is not a batch, and
    /// the singular refusal is what everything already reads.
    #[error(
        "{} records were refused: {}",
        refusals.len(),
        refusals.iter().map(ToString::to_string).collect::<Vec<_>>().join("; ")
    )]
    RecordsRefused {
        /// One refusal per record that disagreed, in the order the commit
        /// walked them.
        refusals: Vec<Error>,
    },

    /// `SPLIT AT` on a table whose records the store numbers with a counter
    /// (G031 S1.2, ADR-0080).
    ///
    /// A counter is one row the whole store shares, so every generated identity
    /// would route its insert through the store's own log and leader, and two
    /// shards written by two nodes could not agree on the next number.
    #[error(
        "table `{table}` is split, so its records cannot be numbered by a counter \
         the whole store shares — declare it `IDENTITY uuid`, or name each record"
    )]
    SplitNeedsGeneratedUuid {
        /// The table being declared.
        table: String,
    },

    /// `SPLIT AT` points that do not ascend strictly in key order.
    ///
    /// Refused rather than sorted, because a list sorted for its author accepts
    /// a boundary they wrote in the wrong place and says nothing.
    #[error(
        "table `{table}`'s split point {position} does not sort after the one \
         before it — write the points once each, in key order"
    )]
    SplitPointsOutOfOrder {
        /// The table being declared.
        table: String,
        /// The offending point, counted from one in written order.
        position: usize,
    },

    /// `SPLIT AT` on a table kind whose rows are not records a caller writes by
    /// identity — an edge, a bucket, a vault, a queue, a view, a series.
    #[error(
        "table `{table}` is {kind}, and only a table declared with `DEFINE TABLE` \
         can be split by the identities of its records"
    )]
    SplitOnAKindThatIsNotRecords {
        /// The table being declared.
        table: String,
        /// What it is instead, as a phrase.
        kind: &'static str,
    },

    /// The parent a catalog entry was to be created under does not exist.
    #[error("no such {entity}: {id}")]
    NoSuchParent {
        /// Which level was missing — `namespace` or `database`.
        entity: &'static str,
        /// The id that resolved to nothing.
        id: u32,
    },

    /// A stored catalog entry does not have the shape a definition needs.
    ///
    /// The bytes decoded as a value, so this is not a codec failure: something
    /// wrote a well-formed value that is not a definition, which makes it an
    /// integrity problem rather than a compatibility one.
    #[error(
        "catalog entry for {} {entity} has a malformed {field}: found {found}",
        article(entity)
    )]
    CatalogMalformed {
        /// Which kind of entry it was.
        entity: &'static str,
        /// The field that was wrong or missing.
        field: &'static str,
        /// The type found in its place, or `none`.
        found: &'static str,
    },

    /// Every identifier at this level has been handed out.
    ///
    /// Ids are never reused after a drop, so the space is consumed by creations
    /// rather than by live entries. Retrying cannot succeed; the store needs a
    /// wider identifier, which is a format change.
    #[error("the {level} id space is exhausted")]
    IdSpaceExhausted {
        /// The level whose counter reached its end.
        level: &'static str,
    },

    /// A read was asked for a point the store can no longer answer exactly.
    ///
    /// Reclamation removed the versions that stood there. Answering anyway
    /// would return an older value, or none, and present it as the state at the
    /// asked-for point — a wrong answer indistinguishable from a right one,
    /// which is the one outcome a historical read must not have.
    #[error(
        "sequence {asked} is below the reclaim floor {floor}; the versions that \
         answered there have been removed"
    )]
    VersionReclaimed {
        /// The sequence the read asked for.
        asked: u64,
        /// The oldest sequence still answerable exactly.
        floor: u64,
    },

    /// A log read was asked for a position that has been pruned away.
    ///
    /// The sibling of [`Self::VersionReclaimed`], one keyspace over, and refused
    /// for the same reason with one difference that matters more here: a
    /// historical read below the reclaim floor returns a *wrong* answer, while a
    /// log read below the start returns a **short** one — and a short answer is
    /// how this protocol says *you are level*. A follower told it is level while
    /// it is missing the records that were pruned would stop asking, and nothing
    /// anywhere would be in an error state.
    ///
    /// So it is refused rather than answered, and the refusal carries the repair
    /// as well as the number: there is no position to retry from, and the only
    /// way back is a fresh copy of the state. That is Raft's `InstallSnapshot`,
    /// etcd's *take a new snapshot and watch from `revision + 1`*, and
    /// PostgreSQL's *re-create the standby* — every system that prunes a log
    /// answers this case with state rather than with more history.
    #[error(
        "sequence {asked} is below the start of this log, which begins at \
         {start}; the records that answered there have been pruned, so this \
         reader cannot catch up and needs a fresh copy of the state"
    )]
    BelowLogStart {
        /// The sequence the read asked for.
        asked: u64,
        /// The oldest sequence this log still holds.
        start: u64,
    },

    /// A read was asked for a point the store has not reached.
    ///
    /// Serving the present instead would let the same query return one answer
    /// now and a different one later while naming the same version, which makes
    /// a historical read non-reproducible — the property it exists to have.
    #[error("sequence {asked} is ahead of the committed tail {tail}")]
    VersionInTheFuture {
        /// The sequence the read asked for.
        asked: u64,
        /// The newest sequence the store has committed.
        tail: u64,
    },

    /// The operating system's randomness source could not be read.
    ///
    /// The store refuses to open rather than falling back to something
    /// predictable. A node id that might collide is worse than a node that will
    /// not start: the collision surfaces as two processes claiming one identity
    /// and every routing decision made from it being wrong with nothing
    /// reporting it, while a refusal surfaces here, once, with this message.
    /// The field is `path` and not `source` because `thiserror` reads a field of
    /// that name as the underlying error rather than as data.
    #[error(
        "cannot read {path}: a node identity must be unpredictable, so this store \
         will not open without one ({reason})"
    )]
    NoEntropy {
        /// The randomness source that could not be read.
        path: &'static str,
        /// What the operating system said.
        reason: String,
    },

    /// The store holds no node identity, so there is nothing to configure.
    ///
    /// Unreachable through an open store, which resolves the identity before it
    /// hands one out. Named anyway rather than left to an unwrap, because that
    /// guarantee lives in another function and a later edit can weaken it there
    /// without this file changing.
    #[error("this store holds no node identity, so there is nothing to configure")]
    NoIdentity,

    /// A failure from the key-value substrate.
    #[error(transparent)]
    Kv(#[from] tessari_kv::Error),

    /// A failure decoding stored bytes.
    #[error(transparent)]
    Encoding(#[from] tessari_encoding::Error),
}

impl Error {
    /// The category this error belongs to.
    #[must_use]
    pub fn category(&self) -> ErrorCategory {
        match self {
            Self::Conflict { .. }
            | Self::LogDivergence { .. }
            | Self::ConcurrentVersions { .. } => ErrorCategory::Conflict,
            Self::CommitContention { .. } => ErrorCategory::Busy,
            // Unavailable rather than Busy or Conflict, because it is the only
            // one of the three that is true: the write was not wrong and
            // retrying *here* will not help, but the cluster may well accept it
            // somewhere else a moment from now.
            Self::LeaseSpent { .. }
            | Self::NoLeadershipYet
            | Self::WriteIsElsewhere { .. } => ErrorCategory::Unavailable,
            Self::LogGap { .. }
            | Self::NameTaken { .. }
            | Self::NoSuchParent { .. }
            | Self::EmptyIndex { .. }
            | Self::UniqueViolation { .. }
            | Self::AssertionViolation { .. }
            | Self::SchemaViolation { .. }
            | Self::MissingRequiredField { .. }
            | Self::UndeclaredField { .. }
            // Every refusal it carries is a validation refusal — nothing else is
            // ever collected into it — so it does not need to look inside.
            | Self::RecordsRefused { .. }
            | Self::IdSpaceExhausted { .. }
            | Self::SplitNeedsGeneratedUuid { .. }
            | Self::SplitPointsOutOfOrder { .. }
            | Self::SplitOnAKindThatIsNotRecords { .. }
            // Validation and not `Unavailable`: the store is healthy and the
            // sequence asked for is the thing that is wrong. Retrying the same
            // read cannot succeed, and a floor only ever rises, so a caller that
            // treated this as transient would retry forever.
            | Self::VersionReclaimed { .. }
            // The same argument as `VersionReclaimed`, and the start only ever
            // rises too: a caller that read this as transient would retry a
            // position that can never come back.
            | Self::BelowLogStart { .. }
            | Self::VersionInTheFuture { .. } => ErrorCategory::Validation,
            Self::CatalogMalformed { .. } => ErrorCategory::Corruption,
            // A dependency this process needs is not reachable, which is what
            // `Unavailable` names. Not `Internal`: nothing here is a bug in the
            // store, and not `Validation`: no caller supplied anything wrong.
            Self::NoEntropy { .. } => ErrorCategory::Unavailable,
            // Corruption rather than `Internal`: the store opened, so an
            // identity was written, and a key that has since gone is the
            // substrate having lost something it acknowledged.
            Self::NoIdentity => ErrorCategory::Corruption,
            // Split by what the caller can do about it, which is the whole
            // job of a category. A sealed store, a wrong passphrase and a
            // second unseal are all things the caller got wrong. Bytes that do
            // not parse as a sealed value were written by something that knew a
            // format this build does not, which is corruption from here. And a
            // refused entropy source is a dependency being unreachable, the
            // same reading `NoEntropy` already takes.
            Self::Vault(inner) => match inner {
                tessari_vault::Error::Entropy => ErrorCategory::Unavailable,
                tessari_vault::Error::NotSealed
                | tessari_vault::Error::UnknownVersion(_)
                | tessari_vault::Error::UnknownAlgorithm(_) => ErrorCategory::Corruption,
                tessari_vault::Error::WrongKey
                | tessari_vault::Error::Sealed
                | tessari_vault::Error::AlreadyUnsealed
                | tessari_vault::Error::Derivation => ErrorCategory::Validation,
            },
            Self::VaultUnavailable | Self::AuditUnavailable { .. } => ErrorCategory::Unavailable,
            // All three are the caller's statement being wrong about the store,
            // not the store being broken: a reserved name it may not write, a
            // shape a vault does not hold, or a record that predates the vault
            // it now sits in.
            // An operator's stated policy failing a range or a relation check
            // is the statement being wrong, not the store being broken — the
            // same reading as a reserved name below.
            Self::FailoverPeriodTooShort { .. }
            | Self::FailoverCampaignOutpaced { .. }
            | Self::FailoverLeaseTooShort { .. }
            | Self::FailoverCollectionAboveFloor { .. }
            | Self::VaultReservedField { .. }
            | Self::VaultNotAnObject { .. }
            | Self::VaultReservedRecipient { .. }
            | Self::VaultRecipientExists { .. }
            | Self::VaultNoRecipient { .. }
            | Self::VaultNoKey { .. } => ErrorCategory::Validation,
            Self::Kv(inner) => inner.category(),
            Self::Encoding(inner) => inner.category(),
        }
    }

    /// Stable machine-readable code for this error.
    #[must_use]
    pub fn code(&self) -> &'static str {
        self.category().code()
    }

    /// Whether retrying the same operation can plausibly succeed.
    ///
    /// A conflict is **not** retryable: the transaction's reads are stale, so
    /// re-running it needs a fresh snapshot and a fresh decision, not a repeat.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        self.category().is_retryable()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_conflict_is_not_retryable_and_names_both_sequences() {
        let error = Error::Conflict {
            id: RecordId::from("r"),
            snapshot: Sequence::new(5),
            committed: Sequence::new(9),
        };
        assert_eq!(error.category(), ErrorCategory::Conflict);
        assert!(!error.is_retryable());
        let text = error.to_string();
        assert!(text.contains('5'), "{text}");
        assert!(text.contains('9'), "{text}");
    }

    #[test]
    fn contention_is_retryable_because_the_transaction_itself_is_still_valid() {
        assert!(Error::CommitContention { attempts: 8 }.is_retryable());
    }
}
