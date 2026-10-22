//! One group's state: its members, its generation and the round it is in.
//!
//! Every method here is synchronous and runs under the group's lock; waiting
//! for the other members is the coordinator's, outside the lock.

use tessari_constants::{
    KAFKA_GROUP_MAX_MEMBERS, KAFKA_GROUP_MEMBER_BYTES, KAFKA_INITIAL_REBALANCE_DELAY_MILLIS,
};
use tokio::time::{Duration, Instant};

use crate::api::join_group::{self, Answer as Joined, Request};

mod settling;
use crate::surface::errors;

/// Where a group stands, named as the protocol's tools name it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Phase {
    /// No members.
    Empty,
    /// Waiting for the members to rejoin.
    Preparing,
    /// Joined; waiting for the leader's assignments.
    Completing,
    /// Assigned and consuming.
    Stable,
}

impl Phase {
    /// The state's name in ListGroups and DescribeGroups.
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Empty => "Empty",
            Self::Preparing => "PreparingRebalance",
            Self::Completing => "CompletingRebalance",
            Self::Stable => "Stable",
        }
    }
}

/// One member.
pub(crate) struct Member {
    pub(crate) id: String,
    pub(crate) instance: Option<String>,
    pub(crate) client: String,
    pub(crate) host: String,
    session: Duration,
    rebalance: Duration,
    protocols: Vec<(String, Vec<u8>)>,
    /// Rejoined in the round under way.
    joined: bool,
    pub(crate) assignment: Vec<u8>,
    seen: Instant,
}

impl Member {
    /// Its subscription under `protocol`.
    pub(crate) fn metadata(&self, protocol: Option<&str>) -> &[u8] {
        self.protocols
            .iter()
            .find(|(name, _)| Some(name.as_str()) == protocol)
            .map_or(&[][..], |(_, metadata)| metadata)
    }

    fn speaks(&self, protocol: &str) -> bool {
        self.protocols.iter().any(|(name, _)| name == protocol)
    }
}

/// What one round's completion told its members.
struct Completed {
    round: u64,
    generation: i32,
    protocol: Option<String>,
    leader: String,
    members: Vec<join_group::Member>,
}

/// What a join leaves the coordinator to do.
pub(crate) enum Entered {
    /// Answered at once: an error, or a first join told its id.
    Answer(Joined),
    /// Joined the round; the answer comes when the round completes.
    Waiting { round: u64, member: String },
}

/// Who asks to join, beside the request itself.
pub(crate) struct Joining<'a> {
    pub(crate) request: &'a Request<'a>,
    pub(crate) version: i16,
    pub(crate) client: &'a str,
    pub(crate) host: &'a str,
    /// A fresh id, made beforehand for a first join.
    pub(crate) fresh: Option<String>,
}

/// One group.
pub(crate) struct State {
    pub(crate) phase: Phase,
    pub(crate) generation: i32,
    pub(crate) protocol_type: Option<String>,
    pub(crate) protocol: Option<String>,
    pub(crate) members: Vec<Member>,
    leader: Option<String>,
    /// Ids handed out by `MEMBER_ID_REQUIRED`, each until it lapses.
    pending: Vec<(String, Instant)>,
    round: u64,
    started: Instant,
    deadline: Instant,
    /// A new group's first round, which waits for more members to arrive.
    delayed: bool,
    completed: Option<Completed>,
    /// Let go from the coordinator; a join that finds this looks again.
    pub(crate) retired: bool,
}

const fn joined_error(code: i16, member: String) -> Joined {
    Joined {
        error_code: code,
        generation: -1,
        protocol_type: None,
        protocol: None,
        leader: String::new(),
        member,
        members: Vec::new(),
    }
}

fn millis(value: i32) -> Duration {
    Duration::from_millis(u64::try_from(value).unwrap_or(0))
}

impl State {
    pub(crate) fn new(now: Instant) -> Self {
        Self {
            phase: Phase::Empty,
            generation: 0,
            protocol_type: None,
            protocol: None,
            members: Vec::new(),
            leader: None,
            pending: Vec::new(),
            round: 0,
            started: now,
            deadline: now,
            delayed: false,
            completed: None,
            retired: false,
        }
    }

    /// When the round under way gives up on members that have not rejoined.
    pub(crate) const fn deadline(&self) -> Instant {
        self.deadline
    }

    fn longest_rebalance(&self) -> Duration {
        self.members
            .iter()
            .map(|member| member.rebalance)
            .max()
            .unwrap_or_default()
    }

    fn start_round(&mut self, now: Instant) {
        let first = self.phase == Phase::Empty;
        self.phase = Phase::Preparing;
        self.round = self.round.wrapping_add(1);
        self.started = now;
        for member in &mut self.members {
            member.joined = false;
        }
        self.delayed = first;
        let wait = if first {
            Duration::from_millis(KAFKA_INITIAL_REBALANCE_DELAY_MILLIS)
                .min(self.longest_rebalance())
        } else {
            self.longest_rebalance()
        };
        self.deadline = now.checked_add(wait).unwrap_or(now);
    }

    /// Take a join.
    pub(crate) fn enter(&mut self, joining: Joining<'_>, now: Instant) -> Entered {
        let request = joining.request;
        if let Some(code) = self.refuses_protocols(request) {
            return Entered::Answer(joined_error(code, request.member.to_owned()));
        }
        let held = request.protocols.iter().fold(0_usize, |sum, protocol| {
            sum.saturating_add(protocol.name.len())
                .saturating_add(protocol.metadata.len())
        });
        if held > KAFKA_GROUP_MEMBER_BYTES {
            return Entered::Answer(joined_error(
                errors::INVALID_REQUEST,
                request.member.to_owned(),
            ));
        }
        let id = if request.member.is_empty() {
            let Some(fresh) = joining.fresh else {
                return Entered::Answer(joined_error(
                    errors::COORDINATOR_NOT_AVAILABLE,
                    String::new(),
                ));
            };
            if joining.version >= 4 {
                if self.pending.len() >= KAFKA_GROUP_MAX_MEMBERS {
                    return Entered::Answer(joined_error(
                        errors::GROUP_MAX_SIZE_REACHED,
                        String::new(),
                    ));
                }
                // The client rejoins with this id, which is how a broker
                // tells a retried first join from a second member.
                let lapses = now
                    .checked_add(millis(request.session_timeout_ms))
                    .unwrap_or(now);
                self.pending.push((fresh.clone(), lapses));
                return Entered::Answer(joined_error(errors::MEMBER_ID_REQUIRED, fresh));
            }
            fresh
        } else if self
            .members
            .iter()
            .any(|member| member.id == request.member)
        {
            request.member.to_owned()
        } else if let Some(at) = self
            .pending
            .iter()
            .position(|(pending, _)| pending == request.member)
        {
            self.pending.swap_remove(at).0
        } else {
            return Entered::Answer(joined_error(
                errors::UNKNOWN_MEMBER_ID,
                request.member.to_owned(),
            ));
        };
        let protocols = request
            .protocols
            .iter()
            .map(|protocol| (protocol.name.to_owned(), protocol.metadata.to_vec()))
            .collect();
        let rebalance = millis(request.rebalance_timeout_ms.clamp(
            request.session_timeout_ms,
            tessari_constants::KAFKA_MAX_SESSION_TIMEOUT_MILLIS,
        ));
        if let Some(member) = self.members.iter_mut().find(|member| member.id == id) {
            member.protocols = protocols;
            member.session = millis(request.session_timeout_ms);
            member.rebalance = rebalance;
            member.seen = now;
        } else {
            if self.members.len() >= KAFKA_GROUP_MAX_MEMBERS {
                return Entered::Answer(joined_error(errors::GROUP_MAX_SIZE_REACHED, id));
            }
            if self.members.is_empty() {
                self.protocol_type = Some(request.protocol_type.to_owned());
            }
            self.members.push(Member {
                id: id.clone(),
                instance: request.instance.map(str::to_owned),
                client: joining.client.to_owned(),
                host: joining.host.to_owned(),
                session: millis(request.session_timeout_ms),
                rebalance,
                protocols,
                joined: false,
                assignment: Vec::new(),
                seen: now,
            });
        }
        if self.phase == Phase::Preparing {
            if self.delayed {
                let limit = self
                    .started
                    .checked_add(self.longest_rebalance())
                    .unwrap_or(now);
                let next = now
                    .checked_add(Duration::from_millis(KAFKA_INITIAL_REBALANCE_DELAY_MILLIS))
                    .unwrap_or(now);
                self.deadline = next.min(limit).max(self.deadline);
            }
        } else {
            self.start_round(now);
        }
        if let Some(member) = self.members.iter_mut().find(|member| member.id == id) {
            member.joined = true;
        }
        self.try_complete(now);
        Entered::Waiting {
            round: self.round,
            member: id,
        }
    }

    /// The code a join is refused with when its protocols cannot join this group.
    fn refuses_protocols(&self, request: &Request<'_>) -> Option<i16> {
        if request.protocol_type.is_empty() || request.protocols.is_empty() {
            return Some(errors::INCONSISTENT_GROUP_PROTOCOL);
        }
        let others: Vec<&Member> = self
            .members
            .iter()
            .filter(|member| member.id != request.member)
            .collect();
        if others.is_empty() {
            return None;
        }
        let same_type = self.protocol_type.as_deref() == Some(request.protocol_type);
        let shared = request
            .protocols
            .iter()
            .any(|protocol| others.iter().all(|member| member.speaks(protocol.name)));
        (!same_type || !shared).then_some(errors::INCONSISTENT_GROUP_PROTOCOL)
    }

    /// Complete the round when every member rejoined, or its time is up.
    pub(crate) fn try_complete(&mut self, now: Instant) -> bool {
        let everyone = !self.delayed && self.members.iter().all(|member| member.joined);
        if self.phase != Phase::Preparing || !(everyone || now >= self.deadline) {
            return false;
        }
        self.members.retain(|member| member.joined);
        self.generation = self.generation.checked_add(1).unwrap_or(1);
        let Some(first) = self.members.first() else {
            self.empty();
            self.completed = Some(Completed {
                round: self.round,
                generation: self.generation,
                protocol: None,
                leader: String::new(),
                members: Vec::new(),
            });
            return true;
        };
        let protocol = first
            .protocols
            .iter()
            .map(|(name, _)| name)
            .find(|name| self.members.iter().all(|member| member.speaks(name)))
            .cloned();
        let leader = match &self.leader {
            Some(leader) if self.members.iter().any(|member| &member.id == leader) => {
                leader.clone()
            }
            _ => first.id.clone(),
        };
        let members = self
            .members
            .iter()
            .map(|member| join_group::Member {
                id: member.id.clone(),
                instance: member.instance.clone(),
                metadata: member.metadata(protocol.as_deref()).to_vec(),
            })
            .collect();
        for member in &mut self.members {
            member.assignment.clear();
        }
        self.phase = Phase::Completing;
        self.protocol.clone_from(&protocol);
        self.leader = Some(leader.clone());
        self.completed = Some(Completed {
            round: self.round,
            generation: self.generation,
            protocol,
            leader,
            members,
        });
        true
    }

    fn empty(&mut self) {
        self.phase = Phase::Empty;
        self.protocol = None;
        self.protocol_type = None;
        self.leader = None;
    }

    /// A waiting join's answer, once its round completed.
    pub(crate) fn joined(&self, round: u64, member: &str) -> Option<Joined> {
        let completed = self.completed.as_ref()?;
        if completed.round < round {
            return (self.phase != Phase::Preparing)
                .then(|| joined_error(errors::REBALANCE_IN_PROGRESS, member.to_owned()));
        }
        if completed.round > round {
            return Some(joined_error(
                errors::REBALANCE_IN_PROGRESS,
                member.to_owned(),
            ));
        }
        if !completed.members.iter().any(|known| known.id == member) {
            return Some(joined_error(errors::UNKNOWN_MEMBER_ID, member.to_owned()));
        }
        let leads = completed.leader == member;
        Some(Joined {
            error_code: errors::NONE,
            generation: completed.generation,
            protocol_type: self.protocol_type.clone(),
            protocol: completed.protocol.clone(),
            leader: completed.leader.clone(),
            member: member.to_owned(),
            members: if leads {
                completed.members.clone()
            } else {
                Vec::new()
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::join_group::Protocol;

    #[test]
    fn the_ids_a_first_join_is_handed_are_bounded() {
        let now = Instant::now();
        let mut state = State::new(now);
        let later = now.checked_add(Duration::from_secs(60)).unwrap();
        for at in 0..KAFKA_GROUP_MAX_MEMBERS {
            state.pending.push((format!("pending-{at}"), later));
        }
        let request = Request {
            group: "g",
            session_timeout_ms: 10_000,
            rebalance_timeout_ms: 10_000,
            member: "",
            instance: None,
            protocol_type: "consumer",
            protocols: vec![Protocol {
                name: "range",
                metadata: b"x",
            }],
        };
        let joining = Joining {
            request: &request,
            version: 7,
            client: "c",
            host: "h",
            fresh: Some("fresh".to_owned()),
        };
        let code = match state.enter(joining, now) {
            Entered::Answer(answer) => Some(answer.error_code),
            Entered::Waiting { .. } => None,
        };
        assert_eq!(
            code,
            Some(errors::GROUP_MAX_SIZE_REACHED),
            "answered, never waiting"
        );
        assert_eq!(
            state.pending.len(),
            KAFKA_GROUP_MAX_MEMBERS,
            "nothing more is held"
        );
    }
}
