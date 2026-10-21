//! The store's refusals as the protocol's error codes.
//!
//! Branched on the refusal's CLASS, never on its words (ADR-0117), with two
//! names read by their variant because the protocol has codes for exactly
//! them: a topic that does not exist and a message that is too large.

use tessari_types::RefusalClass;
use tessaridb::Error;

use crate::surface::running::Failed;

/// No error.
pub(crate) const NONE: i16 = 0;
/// The server failed in a way the protocol has no code for.
pub(crate) const UNKNOWN_SERVER_ERROR: i16 = -1;
/// The offset asked for is not one the partition holds.
pub(crate) const OFFSET_OUT_OF_RANGE: i16 = 1;
/// No such topic or partition.
pub(crate) const UNKNOWN_TOPIC_OR_PARTITION: i16 = 3;
/// Retriable: the request did not complete in time.
pub(crate) const REQUEST_TIMED_OUT: i16 = 7;
/// Retriable: the coordinator cannot take the group right now.
pub(crate) const COORDINATOR_NOT_AVAILABLE: i16 = 15;
/// A topic name that is not a name.
pub(crate) const INVALID_TOPIC_EXCEPTION: i16 = 17;
/// A member's generation is not the group's.
pub(crate) const ILLEGAL_GENERATION: i16 = 22;
/// A member's protocols share nothing with the group's.
pub(crate) const INCONSISTENT_GROUP_PROTOCOL: i16 = 23;
/// An empty group id.
pub(crate) const INVALID_GROUP_ID: i16 = 24;
/// A member the group does not hold.
pub(crate) const UNKNOWN_MEMBER_ID: i16 = 25;
/// A session timeout outside the bounds.
pub(crate) const INVALID_SESSION_TIMEOUT: i16 = 26;
/// The group is rebalancing; the member rejoins.
pub(crate) const REBALANCE_IN_PROGRESS: i16 = 27;
/// Not permitted to take part in a group.
pub(crate) const GROUP_AUTHORIZATION_FAILED: i16 = 30;
/// A topic of that name already exists.
pub(crate) const TOPIC_ALREADY_EXISTS: i16 = 36;
/// A partition count this node cannot serve.
pub(crate) const INVALID_PARTITIONS: i16 = 37;
/// A replication factor this node cannot serve.
pub(crate) const INVALID_REPLICATION_FACTOR: i16 = 38;
/// A topic configuration this node cannot honour.
pub(crate) const INVALID_CONFIG: i16 = 40;
/// A group that still has members cannot be deleted.
pub(crate) const NON_EMPTY_GROUP: i16 = 68;
/// A group this node does not hold.
pub(crate) const GROUP_ID_NOT_FOUND: i16 = 69;
/// A first join: the member rejoins with the id it is given.
pub(crate) const MEMBER_ID_REQUIRED: i16 = 79;
/// The group holds as many members as it may.
pub(crate) const GROUP_MAX_SIZE_REACHED: i16 = 81;
/// A message larger than the topic takes.
pub(crate) const MESSAGE_TOO_LARGE: i16 = 10;
/// Not permitted on this topic.
pub(crate) const TOPIC_AUTHORIZATION_FAILED: i16 = 29;
/// The request is malformed for this broker.
pub(crate) const INVALID_REQUEST: i16 = 42;
/// A producer's batch out of sequence.
pub(crate) const OUT_OF_ORDER_SEQUENCE_NUMBER: i16 = 45;
/// A producer's batch already written; the client treats it as written.
pub(crate) const DUPLICATE_SEQUENCE_NUMBER: i16 = 46;
/// Retriable: the store is throttling this caller.
pub(crate) const THROTTLING_QUOTA_EXCEEDED: i16 = 89;

/// The code a failed topic statement answers with.
pub(crate) fn code(failed: &Failed) -> i16 {
    match failed {
        Failed::Busy => REQUEST_TIMED_OUT,
        Failed::Lost => UNKNOWN_SERVER_ERROR,
        Failed::Refused(refused) => refusal(refused),
        Failed::Carried(refused) => by_class(refused.class),
    }
}

fn refusal(refused: &Error) -> i16 {
    let name = refusal_name(refused);
    if matches!(name, Some("NotATopic" | "Unknown")) {
        return UNKNOWN_TOPIC_OR_PARTITION;
    }
    if matches!(name, Some("TopicMessageTooLarge" | "TopicRetainExceeded")) {
        return MESSAGE_TOO_LARGE;
    }
    by_class(refused.class())
}

/// The code a refusal answers with, by its class alone.
const fn by_class(class: RefusalClass) -> i16 {
    match class {
        RefusalClass::Unauthenticated | RefusalClass::Forbidden => TOPIC_AUTHORIZATION_FAILED,
        RefusalClass::Retry | RefusalClass::Unavailable | RefusalClass::Elsewhere => {
            REQUEST_TIMED_OUT
        }
        RefusalClass::Throttled => THROTTLING_QUOTA_EXCEEDED,
        RefusalClass::Invalid | RefusalClass::Conflict => INVALID_REQUEST,
        _ => UNKNOWN_SERVER_ERROR,
    }
}

/// The variant's name, for the two the protocol has codes for.
fn refusal_name(refused: &Error) -> Option<&'static str> {
    match refused {
        Error::NotATopic { .. } => Some("NotATopic"),
        // A namespace, a database or a table the catalog does not hold: each
        // is a topic that does not exist, from the client's side.
        Error::Unknown {
            entity: "namespace" | "database" | "table",
            ..
        } => Some("Unknown"),
        Error::Store(tessaridb::StoreError::TopicMessageTooLarge { .. }) => {
            Some("TopicMessageTooLarge")
        }
        Error::Store(tessaridb::StoreError::TopicRetainExceeded { .. }) => {
            Some("TopicRetainExceeded")
        }
        _ => None,
    }
}
