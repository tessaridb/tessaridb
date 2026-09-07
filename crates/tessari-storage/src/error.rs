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
use tessari_types::{FieldKind, RecordId, Sequence};

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
    #[error("catalog entry for a {entity} has a malformed {field}: found {found}")]
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
            Self::Conflict { .. } => ErrorCategory::Conflict,
            Self::CommitContention { .. } => ErrorCategory::Busy,
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
            // Validation and not `Unavailable`: the store is healthy and the
            // sequence asked for is the thing that is wrong. Retrying the same
            // read cannot succeed, and a floor only ever rises, so a caller that
            // treated this as transient would retry forever.
            | Self::VersionReclaimed { .. }
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
            Self::VaultUnavailable => ErrorCategory::Unavailable,
            // All three are the caller's statement being wrong about the store,
            // not the store being broken: a reserved name it may not write, a
            // shape a vault does not hold, or a record that predates the vault
            // it now sits in.
            Self::VaultReservedField { .. }
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
