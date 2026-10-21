//! A group request sent to a node that does not coordinate the group: answered
//! NOT_COORDINATOR, so the client asks FindCoordinator again (G079 W5d).
//!
//! A group's membership is node memory, so two members joining through two
//! nodes would form two groups and both be handed every partition. One node
//! coordinates — the one FindCoordinator names — and every other refuses the
//! requests that read or move that memory, offsets included, because a commit
//! is checked against the member's generation.

use std::sync::Arc;

use crate::api::{committed, join_group, membership};
use crate::codec::{Reader, Writer};
use crate::surface::errors::NOT_COORDINATOR;
use crate::surface::grouping::{
    From, Grouped, HEARTBEAT, JOIN_GROUP, LEAVE_GROUP, OFFSET_COMMIT, OFFSET_FETCH, SYNC_GROUP,
};
use crate::surface::running::Held;
use crate::surface::{Shared, metadata};

/// The refusal written for `from`'s request when it is a group request and
/// another node coordinates; `None` when this node answers it.
pub(crate) async fn elsewhere(
    shared: &Arc<Shared>,
    held: &mut Held,
    from: &From<'_>,
    reader: &mut Reader<'_>,
    writer: &mut Writer,
) -> Option<Grouped> {
    let key = from.header.key;
    if !matches!(
        key,
        JOIN_GROUP | SYNC_GROUP | HEARTBEAT | LEAVE_GROUP | OFFSET_COMMIT | OFFSET_FETCH
    ) {
        return None;
    }
    let brokers = metadata::brokers(shared, held, from.reached).await;
    if brokers.coordinator(shared).await.node_id == brokers.this().node_id {
        return None;
    }
    let (version, flexible) = (from.header.version, from.header.flexible);
    let written = match key {
        JOIN_GROUP => {
            let Ok(request) = join_group::Request::read(reader, version, flexible) else {
                return Some(Grouped::Unreadable);
            };
            let refused = join_group::Answer {
                error_code: NOT_COORDINATOR,
                generation: -1,
                member: request.member.to_owned(),
                ..join_group::Answer::default()
            };
            join_group::write_response(writer, version, flexible, &refused)
        }
        SYNC_GROUP => {
            if membership::Sync::read(reader, version, flexible).is_err() {
                return Some(Grouped::Unreadable);
            }
            let refused = membership::Synced {
                error_code: NOT_COORDINATOR,
                ..membership::Synced::default()
            };
            membership::write_sync(writer, version, flexible, &refused)
        }
        HEARTBEAT => {
            if membership::read_heartbeat(reader, version, flexible).is_err() {
                return Some(Grouped::Unreadable);
            }
            membership::write_heartbeat(writer, version, flexible, NOT_COORDINATOR);
            Ok(())
        }
        LEAVE_GROUP => {
            let Ok(request) = membership::Leave::read(reader, version, flexible) else {
                return Some(Grouped::Unreadable);
            };
            let members: Vec<_> = request
                .members
                .iter()
                .map(|(member, instance)| (*member, *instance, NOT_COORDINATOR))
                .collect();
            membership::write_leave(writer, version, flexible, NOT_COORDINATOR, &members)
        }
        OFFSET_COMMIT => {
            let Ok(request) = committed::CommitRequest::read(reader, version, flexible) else {
                return Some(Grouped::Unreadable);
            };
            let topics: Vec<_> = request
                .topics
                .iter()
                .map(|(topic, commits)| {
                    let codes = commits
                        .iter()
                        .map(|commit| (commit.index, NOT_COORDINATOR))
                        .collect();
                    ((*topic).to_owned(), codes)
                })
                .collect();
            committed::write_commit(writer, version, flexible, &topics)
        }
        OFFSET_FETCH => {
            let Ok(request) = committed::FetchRequest::read(reader, version, flexible) else {
                return Some(Grouped::Unreadable);
            };
            let topics: Vec<_> = request
                .topics
                .iter()
                .flatten()
                .map(|(topic, partitions)| {
                    let refused = partitions
                        .iter()
                        .map(|index| committed::Committed {
                            index: *index,
                            offset: -1,
                            error_code: NOT_COORDINATOR,
                        })
                        .collect();
                    ((*topic).to_owned(), refused)
                })
                .collect();
            committed::write_fetch(writer, version, flexible, NOT_COORDINATOR, &topics)
        }
        _ => return None,
    };
    Some(Grouped::Written(written))
}
