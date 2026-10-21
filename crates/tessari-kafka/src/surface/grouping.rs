//! The group and topic-administration requests: read, answered, written.

use std::net::SocketAddr;
use std::sync::Arc;

use tessaridb::Parameters;
use tokio::time::Duration;

use crate::api::{committed, coordinator, groups, join_group, membership, topics};
use crate::codec::{Reader, Writer};
use crate::error::Result;
use crate::header::RequestHeader;
use crate::surface::coordinator::Joiner;
use crate::surface::running::Held;
use crate::surface::{Shared, administering, committing, errors, metadata, placing};

/// How long a member's sync waits for the leader's assignments. The leader
/// that never sends them is let go at its session timeout, which ends the
/// wait sooner with a rebalance.
const SYNC_PATIENCE: Duration = Duration::from_secs(300);

/// What one of these requests came to.
pub(crate) enum Grouped {
    /// Not one of these keys.
    NotMine,
    /// Answered into the writer, or the writer refused.
    Written(Result<()>),
    /// The body could not be read: the connection closes.
    Unreadable,
}

pub(crate) const OFFSET_COMMIT: i16 = 8;
pub(crate) const OFFSET_FETCH: i16 = 9;
pub(crate) const FIND_COORDINATOR: i16 = 10;
pub(crate) const JOIN_GROUP: i16 = 11;
pub(crate) const HEARTBEAT: i16 = 12;
pub(crate) const LEAVE_GROUP: i16 = 13;
pub(crate) const SYNC_GROUP: i16 = 14;
pub(crate) const DESCRIBE_GROUPS: i16 = 15;
pub(crate) const LIST_GROUPS: i16 = 16;
pub(crate) const CREATE_TOPICS: i16 = 19;
pub(crate) const DELETE_TOPICS: i16 = 20;
pub(crate) const DELETE_GROUPS: i16 = 42;

/// Where the request came from, for the coordinator's answers.
pub(crate) struct From<'a> {
    pub(crate) header: &'a RequestHeader<'a>,
    pub(crate) reached: Option<SocketAddr>,
    pub(crate) peer: Option<SocketAddr>,
}

/// Whether an anonymous session may act: only on a store with no users,
/// which answers anything for anyone.
pub(crate) async fn store_is_open(shared: &Arc<Shared>, held: &mut Held) -> bool {
    held.run_at(shared, None, None, "INFO FOR STORE;", &Parameters::new())
        .await
        .is_ok()
}

/// Answer `from`'s request when it is one of these.
pub(crate) async fn answer(
    shared: &Arc<Shared>,
    held: &mut Held,
    from: &From<'_>,
    reader: &mut Reader<'_>,
    writer: &mut Writer,
) -> Grouped {
    let (version, flexible) = (from.header.version, from.header.flexible);
    if let Some(refused) = placing::elsewhere(shared, held, from, reader, writer).await {
        return refused;
    }
    let written = match from.header.key {
        FIND_COORDINATOR => {
            let Ok(request) = coordinator::Request::read(reader, version, flexible) else {
                return Grouped::Unreadable;
            };
            let brokers = metadata::brokers(shared, held, from.reached).await;
            let broker = brokers.coordinator(shared).await;
            let answers: Vec<_> = request
                .keys
                .iter()
                .map(|key| {
                    if request.key_type == coordinator::GROUP {
                        coordinator::Answer {
                            key,
                            error_code: 0,
                            node_id: broker.node_id,
                            host: &broker.host,
                            port: broker.port,
                        }
                    } else {
                        // Transactions are not served; InitProducerId says the same.
                        coordinator::Answer {
                            key,
                            error_code:
                                crate::api::init_producer::TRANSACTIONAL_ID_AUTHORIZATION_FAILED,
                            node_id: -1,
                            host: "",
                            port: -1,
                        }
                    }
                })
                .collect();
            coordinator::write_response(writer, version, flexible, &answers)
        }
        JOIN_GROUP => {
            let Ok(request) = join_group::Request::read(reader, version, flexible) else {
                return Grouped::Unreadable;
            };
            if held.user().is_empty() && !store_is_open(shared, held).await {
                // Membership is node memory; a store that has users does not
                // let a stranger make any.
                let refused = join_group::Answer {
                    error_code: errors::GROUP_AUTHORIZATION_FAILED,
                    generation: -1,
                    member: request.member.to_owned(),
                    ..join_group::Answer::default()
                };
                return Grouped::Written(join_group::write_response(
                    writer, version, flexible, &refused,
                ));
            }
            let host = from
                .peer
                .map(|peer| peer.ip().to_string())
                .unwrap_or_default();
            let joiner = Joiner {
                owner: held.user(),
                client: from.header.client_id.unwrap_or(""),
                host: &host,
                version,
            };
            let answer = shared.coordinator.join(&joiner, &request).await;
            join_group::write_response(writer, version, flexible, &answer)
        }
        SYNC_GROUP => {
            let Ok(request) = membership::Sync::read(reader, version, flexible) else {
                return Grouped::Unreadable;
            };
            let synced = shared
                .coordinator
                .sync(
                    held.user(),
                    &request.place,
                    &request.assignments,
                    SYNC_PATIENCE,
                )
                .await;
            membership::write_sync(writer, version, flexible, &synced)
        }
        HEARTBEAT => {
            let Ok(place) = membership::read_heartbeat(reader, version, flexible) else {
                return Grouped::Unreadable;
            };
            let code = shared.coordinator.heartbeat(held.user(), &place).await;
            membership::write_heartbeat(writer, version, flexible, code);
            Ok(())
        }
        LEAVE_GROUP => {
            let Ok(request) = membership::Leave::read(reader, version, flexible) else {
                return Grouped::Unreadable;
            };
            let ids: Vec<&str> = request.members.iter().map(|(member, _)| *member).collect();
            let codes = shared
                .coordinator
                .leave(held.user(), request.group, &ids)
                .await;
            let members: Vec<_> = request
                .members
                .iter()
                .zip(&codes)
                .map(|((member, instance), code)| (*member, *instance, *code))
                .collect();
            // Before version 3 the one member's code is the answer's.
            let top = if version < 3 {
                codes.first().copied().unwrap_or(0)
            } else {
                0
            };
            membership::write_leave(writer, version, flexible, top, &members)
        }
        OFFSET_COMMIT => {
            let Ok(request) = committed::CommitRequest::read(reader, version, flexible) else {
                return Grouped::Unreadable;
            };
            let answers = committing::commit(shared, held, &request).await;
            committed::write_commit(writer, version, flexible, &answers)
        }
        OFFSET_FETCH => {
            let Ok(request) = committed::FetchRequest::read(reader, version, flexible) else {
                return Grouped::Unreadable;
            };
            let answers = committing::fetch(shared, held, &request).await;
            committed::write_fetch(writer, version, flexible, 0, &answers)
        }
        LIST_GROUPS => {
            let Ok(states) = groups::read_list(reader, version, flexible) else {
                return Grouped::Unreadable;
            };
            let listed = shared.coordinator.list(held.user(), &states).await;
            groups::write_list(writer, version, flexible, &listed)
        }
        DESCRIBE_GROUPS => {
            let Ok(asked) = groups::read_describe(reader, version, flexible) else {
                return Grouped::Unreadable;
            };
            let described = shared.coordinator.describe(held.user(), &asked).await;
            groups::write_describe(writer, version, flexible, &described)
        }
        DELETE_GROUPS => {
            let Ok(asked) = groups::read_delete(reader, flexible) else {
                return Grouped::Unreadable;
            };
            let codes = shared.coordinator.delete(held.user(), &asked).await;
            let results: Vec<_> = asked.iter().copied().zip(codes).collect();
            groups::write_delete(writer, flexible, &results)
        }
        CREATE_TOPICS => {
            let Ok(request) = topics::CreateRequest::read(reader, flexible) else {
                return Grouped::Unreadable;
            };
            let created = administering::create(shared, held, &request).await;
            topics::write_create(writer, version, flexible, &created)
        }
        DELETE_TOPICS => {
            let Ok(asked) = topics::read_delete(reader, flexible) else {
                return Grouped::Unreadable;
            };
            let codes = administering::delete(shared, held, &asked).await;
            let results: Vec<_> = asked.iter().copied().zip(codes).collect();
            topics::write_delete(writer, flexible, &results)
        }
        _ => return Grouped::NotMine,
    };
    Grouped::Written(written)
}
