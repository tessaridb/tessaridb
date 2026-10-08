//! The one place a refusal is classified (ADR-0117).
//!
//! HTTP derives its status from this and the wire sends it as a byte, so the two
//! surfaces cannot disagree about what a caller should do. The match names every
//! refusal, with no catch-all: a refusal added later does not compile until
//! somebody has decided whether a caller may retry it.

use tessari_storage::ErrorCategory;
use tessari_types::RefusalClass;

use crate::error::Error;
use crate::session::{AcrossRefusal, RefusalKind};

impl Error {
    /// What a caller should do about this refusal.
    #[must_use]
    pub fn class(&self) -> RefusalClass {
        match self {
            Self::Script(_) => RefusalClass::Invalid,
            // The class of what failed in the body: a body that met contention
            // is retried like any write, one that wrote a missing table is a
            // request to fix (ADR-0124 D7).
            Self::EventFailed { cause, .. } => cause.class(),
            Self::Store(error) => store(error),
            Self::AcrossAborted { refusal, .. } => match refusal {
                AcrossRefusal::Here(cause) => cause.class(),
                AcrossRefusal::There(part) => match part.kind {
                    RefusalKind::Retriable => RefusalClass::Retry,
                    RefusalKind::Forbidden => RefusalClass::Forbidden,
                    RefusalKind::Invalid => RefusalClass::Invalid,
                },
            },
            Self::NotSignedIn { .. }
            | Self::SignInRefused
            | Self::TicketStale
            | Self::CurrentPasswordRefused => RefusalClass::Unauthenticated,
            Self::SignInThrottled | Self::PassphraseThrottled | Self::TopicRateExceeded { .. } => {
                RefusalClass::Throttled
            }
            Self::RoleForbids { .. }
            | Self::OutsideTenancy { .. }
            | Self::NotGranted { .. }
            | Self::NotTheWholeStore { .. }
            | Self::CannotHandOut { .. }
            | Self::GrantedUserCannotBackUp { .. }
            | Self::GrantedUserCannotDeclare { .. }
            | Self::NotYours { .. }
            | Self::WiderThanYou { .. }
            | Self::MayNotTravel { .. } => RefusalClass::Forbidden,
            Self::ReadIsElsewhere { .. } => RefusalClass::Elsewhere,
            Self::ShardMapMoved { .. } | Self::AcrossSettling { .. } => RefusalClass::Retry,
            Self::RecordExists { .. }
            | Self::StillDepended { .. }
            | Self::BackupExists { .. }
            | Self::RestoreTargetExists { .. }
            | Self::NoVaultRoot
            | Self::FormatNotFinalized { .. }
            | Self::AcrossInDoubt { .. }
            | Self::NotAcknowledgedInTime { .. }
            | Self::ConditionNotMet { .. }
            // The request assumed a record the store does not hold: re-read,
            // as for the compare-and-set beside it (ADR-0124 D7).
            | Self::NoSuchRecord { .. }
            | Self::HeldByAnother { .. }
            | Self::EventExists { .. }
            | Self::ParamExists { .. }
            | Self::SearchExists { .. }
            | Self::GroupExists { .. } => RefusalClass::Conflict,
            Self::NoBackupFolder
            | Self::FormatPeerTooOld { .. }
            | Self::NotWritable { .. }
            | Self::MajorityUnreachable { .. }
            | Self::AcrossUnavailable { .. }
            | Self::NoLeaderKnown { .. }
            | Self::NoCopyWithinStaleness { .. }
            | Self::NotHeldHere { .. }
            | Self::NotGathered { .. } => RefusalClass::Unavailable,
            Self::Encoding(..)
            | Self::BackupFailed { .. }
            | Self::IdentityUnavailable { .. }
            | Self::TokenUnavailable { .. }
            | Self::FoldOutsideAGroup { .. } => RefusalClass::Internal,
            Self::UndeclaredField { .. }
            | Self::Thrown { .. }
            | Self::FailoverRefused { .. }
            | Self::NoSuchAccessPath { .. }
            | Self::TimedOut { .. }
            | Self::Unbounded { .. }
            | Self::UnboundedCollection { .. }
            | Self::NotAlone { .. }
            | Self::AnchorGone { .. }
            | Self::PathNotTaken { .. }
            | Self::IndexNotUsed { .. }
            | Self::JoinKeysDiffer { .. }
            | Self::MergeIsNotAnObject { .. }
            | Self::NoSuchRouteToAssign { .. }
            | Self::WriteWouldLeaveAHole { .. }
            | Self::FileAboveBucketCeiling { .. }
            | Self::BackupNameRefused { .. }
            | Self::RestoreRefused { .. }
            | Self::GeometryRefused { .. }
            | Self::NoNamespaceSelected { .. }
            | Self::NoDatabaseSelected { .. }
            | Self::Unknown { .. }
            | Self::BindingIsNotAValue { .. }
            | Self::NestedTransaction { .. }
            | Self::TransactionVerbInAtomic { .. }
            | Self::NoOpenTransaction { .. }
            | Self::VersionInsideTransaction { .. }
            | Self::UnclosedTransaction { .. }
            | Self::ViewIsNotATable { .. }
            | Self::MaterializedShape { .. }
            | Self::MaterializedFromHidden { .. }
            | Self::ViewsTooDeep { .. }
            | Self::NestedTooDeep { .. }
            | Self::ViewUnreadable { .. }
            | Self::SeriesTimeMissing { .. }
            | Self::SeriesTimeOutOfRange { .. }
            | Self::BelowSeriesFloor { .. }
            | Self::SeriesIdentityDerived { .. }
            | Self::SeriesTimeFixed { .. }
            | Self::FillNeedsWindow { .. }
            | Self::FillNeedsRange { .. }
            | Self::FillTooWide { .. }
            | Self::LatestNeedsSeries { .. }
            | Self::LatestBesideGroup { .. }
            | Self::AsofNeedsTime { .. }
            | Self::RollupNeedsSeries { .. }
            | Self::RollupFold { .. }
            | Self::RollupWindow { .. }
            | Self::RollupIsDerived { .. }
            | Self::RollupInTransaction { .. }
            | Self::RollupKeyCollision { .. }
            | Self::RollupsDependOn { .. }
            | Self::RecipientIsNotAName { .. }
            | Self::NotAnEdgeTable { .. }
            | Self::EndpointsNotDeclared { .. }
            | Self::TableBelongsToGraph { .. }
            | Self::EndpointOutsideGraph { .. }
            | Self::NoHistoricalTraversal { .. }
            | Self::PathWeight { .. }
            | Self::PathOverEdgeTable { .. }
            | Self::GatheredTooMuch { .. }
            | Self::EdgePropertiesNotAnObject { .. }
            | Self::InvalidKeyBound { .. }
            | Self::ConditionNotBoolean { .. }
            | Self::NotArithmetic { .. }
            | Self::ArithmeticFailed { .. }
            | Self::WrongArgument { .. }
            | Self::CallFailed { .. }
            | Self::InvalidExpiry { .. }
            | Self::AcknowledgeBelowNamespace { .. }
            | Self::LocalMajorityWithoutRegion { .. }
            | Self::StalenessBelowFloor { .. }
            | Self::ReplicationUnstated { .. }
            | Self::NotReadBySelect { .. }
            | Self::NotIndexable { .. }
            | Self::SecretNeedsVault { .. }
            | Self::VaultIsStrict { .. }
            | Self::ExpiryNotOnThisKind { .. }
            | Self::TableDoesNotExpire { .. }
            | Self::NotASecret { .. }
            | Self::NotCastable { .. }
            | Self::DefaultDoesNotMatch { .. }
            | Self::NotSummable { .. }
            | Self::ManyWritablePeers { .. }
            | Self::VaultUsesStorePassphrase { .. }
            | Self::NoSearchIndex { .. }
            | Self::PrefixTooShort { .. }
            | Self::MalformedSlop { .. }
            | Self::NegationWithoutTerm { .. }
            | Self::SearchNeedsText { .. }
            | Self::SearchIsItsOwnOrder { .. }
            | Self::NotSearched { .. }
            | Self::EventDepth { .. }
            | Self::EventOnKind { .. }
            | Self::SearchNamesTableTwice { .. }
            | Self::SearchNamesFieldTwice { .. }
            | Self::NotOneWord { .. }
            | Self::WeightOutOfRange { .. }
            | Self::NoSuchDistance { .. }
            | Self::VaultEditComputesFromTheRecord { .. }
            | Self::OutsideTheirTenancy { .. }
            | Self::NotAtThatReach { .. }
            | Self::UnknownUser { .. }
            | Self::NoSuchRole { .. }
            | Self::NoSuchAuthority { .. }
            | Self::PasswordEmpty { .. }
            | Self::DuplicateMapping { .. }
            | Self::NoSuchVerb { .. }
            | Self::AcrossKind { .. }
            | Self::FieldsOnAWrite { .. }
            | Self::LastGrant { .. }
            | Self::PasshashRefused { .. }
            | Self::PasswordUnusable { .. }
            | Self::NoRecordInScope { .. }
            | Self::NotFused { .. }
            | Self::NotABucket { .. }
            | Self::NotAQueue { .. }
            | Self::NotATopic { .. }
            | Self::NoSuchGroup { .. }
            | Self::NotAGroup { .. }
            | Self::AfterOnGroup { .. }
            | Self::DeadLetterIsTheTopic { .. }
            | Self::TopicConsumerSpansDatabases { .. }
            | Self::QuarantineNeedsDeadLetter { .. }
            | Self::WrongConsumerKind { .. }
            | Self::InvalidPosition { .. }
            | Self::QueueFieldIsTheEngines { .. }
            | Self::NoConsumerDeclared { .. }
            | Self::ClaimAboveCeiling { .. }
            | Self::NoDelayField { .. }
            | Self::NotAnInstant { .. }
            | Self::DeduplicationKey { .. }
            | Self::ClaimDeadlineUnreachable { .. }
            | Self::NotWrittenByHand { .. }
            | Self::FileIsNotBytes { .. }
            | Self::FileNeedsAPath { .. }
            | Self::FileIsIncomplete { .. }
            | Self::ParameterHasNoValue { .. } => RefusalClass::Invalid,
        }
    }
}

/// A store refusal, by the category the storage layer already gives it.
fn store(error: &tessari_storage::Error) -> RefusalClass {
    use tessari_storage::Error as Store;
    match error {
        Store::WriteIsElsewhere { .. } => return RefusalClass::Elsewhere,
        // The storage layer files these under validation, and for its purpose
        // they are; for a caller they are the data saying no to a request that
        // was written right — a name already taken, a value already held, a
        // parent not there yet, a space already full. Re-read and decide.
        Store::NameTaken { .. }
        | Store::UniqueViolation { .. }
        | Store::NoSuchParent { .. }
        | Store::SpaceFull { .. } => return RefusalClass::Conflict,
        _ => {}
    }
    match error.category() {
        // A conflict between transactions and a busy engine are both answered by
        // running the transaction again.
        ErrorCategory::Conflict | ErrorCategory::Busy => RefusalClass::Retry,
        ErrorCategory::Unavailable | ErrorCategory::Lifecycle => RefusalClass::Unavailable,
        ErrorCategory::Validation => RefusalClass::Invalid,
        ErrorCategory::Corruption | ErrorCategory::Incompatible | ErrorCategory::Internal => {
            RefusalClass::Internal
        }
    }
}

#[cfg(test)]
mod tests {
    use tessari_types::RefusalClass;

    use crate::error::Error;

    #[test]
    fn each_kind_of_refusal_is_classed_by_what_the_caller_should_do() {
        let store = |error| Error::Store(error);
        for (error, class) in [
            // Run it again: the engine was contended, the map moved.
            (
                store(tessari_storage::Error::CommitContention { attempts: 8 }),
                RefusalClass::Retry,
            ),
            // The data says no to a request written right.
            (
                store(tessari_storage::Error::NameTaken {
                    qualified: "ns/db/t".to_owned(),
                }),
                RefusalClass::Conflict,
            ),
            (Error::SignInThrottled, RefusalClass::Throttled),
            (
                Error::BackupFailed {
                    reason: "the disk said no".to_owned(),
                },
                RefusalClass::Internal,
            ),
            (Error::NoBackupFolder, RefusalClass::Unavailable),
        ] {
            assert_eq!(error.class(), class, "{error}");
        }
    }
}
