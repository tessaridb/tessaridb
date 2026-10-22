//! The group coordinator: membership in node memory (ADR-0129 D6).
//!
//! A group's committed offsets are reader positions in the store; what lives
//! here is who is in the group now and what each was assigned, which the
//! classic protocol rebuilds by itself when a node restarts. Groups are held
//! per store user, because membership is not something the store's grants can
//! govern — scoped by identity, one user can neither rebalance nor read
//! another's group.

mod group;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use dashmap::DashMap;
use tessari_constants::{
    KAFKA_MAX_GROUPS, KAFKA_MAX_SESSION_TIMEOUT_MILLIS, KAFKA_MIN_SESSION_TIMEOUT_MILLIS,
};
use tokio::sync::{Mutex, Notify};
use tokio::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

use crate::api::groups::{Described, DescribedMember, Listed};
use crate::api::join_group::{Answer as Joined, Request};
use crate::api::membership::{Place, Synced};
use crate::surface::errors;

use group::{Entered, Joining, State};

/// How often lapsed members are let go.
const SWEEP: Duration = Duration::from_secs(1);

/// A group: its state and the signal that it moved.
struct Group {
    state: Mutex<State>,
    changed: Notify,
}

/// Every group this node holds, keyed by the owning user and the group id.
pub(crate) struct Coordinator {
    groups: DashMap<(String, String), Arc<Group>>,
    held: AtomicUsize,
    stopping: CancellationToken,
}

/// Who a join comes from, as a description reports it.
pub(crate) struct Joiner<'a> {
    pub(crate) owner: &'a str,
    pub(crate) client: &'a str,
    pub(crate) host: &'a str,
    pub(crate) version: i16,
}

fn refused(code: i16, member: &str) -> Joined {
    Joined {
        error_code: code,
        generation: -1,
        member: member.to_owned(),
        ..Joined::default()
    }
}

/// A member id: the client's name and sixteen random bytes, as a broker makes it.
fn fresh_member(client: &str) -> Option<String> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).ok()?;
    let mut id = String::with_capacity(client.len().saturating_add(33));
    id.push_str(client);
    id.push('-');
    for byte in bytes {
        id.push(char::from_digit(u32::from(byte >> 4), 16)?);
        id.push(char::from_digit(u32::from(byte & 0x0f), 16)?);
    }
    Some(id)
}

impl Coordinator {
    pub(crate) fn new() -> Self {
        Self {
            groups: DashMap::new(),
            held: AtomicUsize::new(0),
            stopping: CancellationToken::new(),
        }
    }

    /// End every wait and the sweeper; the node is stopping.
    pub(crate) fn stop(&self) {
        self.stopping.cancel();
    }

    fn existing(&self, owner: &str, group: &str) -> Option<Arc<Group>> {
        self.groups
            .get(&(owner.to_owned(), group.to_owned()))
            .map(|entry| Arc::clone(entry.value()))
    }

    /// The group, made when it is new; `None` when the node holds as many as it may.
    fn group_for(&self, owner: &str, group: &str) -> Option<Arc<Group>> {
        if let Some(found) = self.existing(owner, group) {
            return Some(found);
        }
        if self.held.load(Ordering::Relaxed) >= KAFKA_MAX_GROUPS && !self.let_one_go() {
            return None;
        }
        let mut made = false;
        let found = Arc::clone(
            self.groups
                .entry((owner.to_owned(), group.to_owned()))
                .or_insert_with(|| {
                    made = true;
                    Arc::new(Group {
                        state: Mutex::new(State::new(Instant::now())),
                        changed: Notify::new(),
                    })
                })
                .value(),
        );
        if made {
            self.held.fetch_add(1, Ordering::Relaxed);
        }
        Some(found)
    }

    /// Let one memberless group go to make room; answers whether one went.
    fn let_one_go(&self) -> bool {
        let mut chosen = None;
        for entry in &self.groups {
            if let Ok(mut state) = entry.value().state.try_lock()
                && state.members.is_empty()
                && !state.retired
            {
                state.retired = true;
                chosen = Some(entry.key().clone());
                break;
            }
        }
        chosen.is_some_and(|key| self.forget(&key))
    }

    fn forget(&self, key: &(String, String)) -> bool {
        let gone = self.groups.remove(key).is_some();
        if gone {
            self.held.fetch_sub(1, Ordering::Relaxed);
        }
        gone
    }

    /// Answer a join, once its round completes.
    pub(crate) async fn join(&self, joiner: &Joiner<'_>, request: &Request<'_>) -> Joined {
        if request.group.is_empty() {
            return refused(errors::INVALID_GROUP_ID, request.member);
        }
        if !(KAFKA_MIN_SESSION_TIMEOUT_MILLIS..=KAFKA_MAX_SESSION_TIMEOUT_MILLIS)
            .contains(&request.session_timeout_ms)
        {
            return refused(errors::INVALID_SESSION_TIMEOUT, request.member);
        }
        let fresh = request
            .member
            .is_empty()
            .then(|| fresh_member(joiner.client))
            .flatten();
        loop {
            let Some(group) = self.group_for(joiner.owner, request.group) else {
                return refused(errors::COORDINATOR_NOT_AVAILABLE, request.member);
            };
            let mut state = group.state.lock().await;
            if state.retired {
                continue;
            }
            let joining = Joining {
                request,
                version: joiner.version,
                client: joiner.client,
                host: joiner.host,
                fresh: fresh.clone(),
            };
            let (round, member) = match state.enter(joining, Instant::now()) {
                Entered::Answer(answer) => return answer,
                Entered::Waiting { round, member } => (round, member),
            };
            drop(state);
            group.changed.notify_waiters();
            return self.until_joined(&group, round, &member).await;
        }
    }

    async fn until_joined(&self, group: &Group, round: u64, member: &str) -> Joined {
        loop {
            let notified = group.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let deadline = {
                let mut state = group.state.lock().await;
                if let Some(answer) = state.joined(round, member) {
                    return answer;
                }
                if state.try_complete(Instant::now()) {
                    group.changed.notify_waiters();
                    if let Some(answer) = state.joined(round, member) {
                        return answer;
                    }
                }
                state.deadline()
            };
            tokio::select! {
                biased;
                () = self.stopping.cancelled() => {
                    return refused(errors::COORDINATOR_NOT_AVAILABLE, member);
                }
                () = &mut notified => {}
                () = tokio::time::sleep_until(deadline) => {}
            }
        }
    }

    /// Answer a sync, once the leader's assignments are in.
    pub(crate) async fn sync(
        &self,
        owner: &str,
        place: &Place<'_>,
        assignments: &[(&str, &[u8])],
        patience: Duration,
    ) -> Synced {
        let unknown = Synced {
            error_code: errors::UNKNOWN_MEMBER_ID,
            ..Synced::default()
        };
        let Some(group) = self.existing(owner, place.group) else {
            return unknown;
        };
        let give_up = Instant::now()
            .checked_add(patience)
            .unwrap_or_else(Instant::now);
        let mut first = true;
        loop {
            let notified = group.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let mut state = group.state.lock().await;
                let answered = if first {
                    state.sync(place, assignments, Instant::now())
                } else {
                    state.synced(place)
                };
                if let Some(answer) = answered {
                    drop(state);
                    if first {
                        group.changed.notify_waiters();
                    }
                    return answer;
                }
            }
            first = false;
            tokio::select! {
                biased;
                () = self.stopping.cancelled() => break,
                () = &mut notified => {}
                () = tokio::time::sleep_until(give_up) => break,
            }
        }
        Synced {
            error_code: errors::REBALANCE_IN_PROGRESS,
            ..Synced::default()
        }
    }

    /// Answer a heartbeat.
    pub(crate) async fn heartbeat(&self, owner: &str, place: &Place<'_>) -> i16 {
        let Some(group) = self.existing(owner, place.group) else {
            return errors::UNKNOWN_MEMBER_ID;
        };
        group.state.lock().await.heartbeat(place, Instant::now())
    }

    /// Answer a leave: a code per member.
    pub(crate) async fn leave(&self, owner: &str, group: &str, members: &[&str]) -> Vec<i16> {
        let Some(found) = self.existing(owner, group) else {
            return vec![errors::UNKNOWN_MEMBER_ID; members.len()];
        };
        let mut state = found.state.lock().await;
        let now = Instant::now();
        let codes = members
            .iter()
            .map(|member| state.leave(member, now))
            .collect();
        drop(state);
        found.changed.notify_waiters();
        codes
    }

    /// Whether a commit from `member` at `generation` is taken now.
    pub(crate) async fn may_commit(
        &self,
        owner: &str,
        group: &str,
        generation: i32,
        member: &str,
    ) -> i16 {
        match self.existing(owner, group) {
            Some(found) => found
                .state
                .lock()
                .await
                .may_commit(generation, member, Instant::now()),
            None if generation < 0 => errors::NONE,
            None => errors::UNKNOWN_MEMBER_ID,
        }
    }

    /// The groups `owner` holds, in the states asked for (every state when none).
    pub(crate) async fn list(&self, owner: &str, states: &[&str]) -> Vec<Listed> {
        let mine: Vec<(String, Arc<Group>)> = self
            .groups
            .iter()
            .filter(|entry| entry.key().0 == owner)
            .map(|entry| (entry.key().1.clone(), Arc::clone(entry.value())))
            .collect();
        let mut listed = Vec::with_capacity(mine.len());
        for (id, group) in mine {
            let state = group.state.lock().await;
            let name = state.phase.name();
            if states.is_empty() || states.iter().any(|asked| asked.eq_ignore_ascii_case(name)) {
                listed.push(Listed {
                    group: id,
                    protocol_type: state.protocol_type.clone().unwrap_or_default(),
                    state: name,
                });
            }
        }
        listed
    }

    /// Describe each group asked about; one this node does not hold is `Dead`.
    pub(crate) async fn describe(&self, owner: &str, asked: &[&str]) -> Vec<Described> {
        let mut described = Vec::with_capacity(asked.len());
        for id in asked {
            let Some(group) = self.existing(owner, id) else {
                described.push(Described {
                    error_code: errors::NONE,
                    group: (*id).to_owned(),
                    state: "Dead",
                    protocol_type: String::new(),
                    protocol: String::new(),
                    members: Vec::new(),
                });
                continue;
            };
            let state = group.state.lock().await;
            let protocol = state.protocol.as_deref();
            described.push(Described {
                error_code: errors::NONE,
                group: (*id).to_owned(),
                state: state.phase.name(),
                protocol_type: state.protocol_type.clone().unwrap_or_default(),
                protocol: protocol.unwrap_or_default().to_owned(),
                members: state
                    .members
                    .iter()
                    .map(|member| DescribedMember {
                        id: member.id.clone(),
                        instance: member.instance.clone(),
                        client: member.client.clone(),
                        host: member.host.clone(),
                        metadata: member.metadata(protocol).to_vec(),
                        assignment: member.assignment.clone(),
                    })
                    .collect(),
            });
        }
        described
    }

    /// Delete the groups asked about: a code per group. Their committed
    /// offsets stay, as reader positions in the store.
    pub(crate) async fn delete(&self, owner: &str, asked: &[&str]) -> Vec<i16> {
        let mut codes = Vec::with_capacity(asked.len());
        for id in asked {
            let key = (owner.to_owned(), (*id).to_owned());
            let Some(group) = self.existing(owner, id) else {
                codes.push(errors::GROUP_ID_NOT_FOUND);
                continue;
            };
            let mut state = group.state.lock().await;
            if !state.members.is_empty() {
                codes.push(errors::NON_EMPTY_GROUP);
                continue;
            }
            state.retired = true;
            drop(state);
            self.forget(&key);
            codes.push(errors::NONE);
        }
        codes
    }

    /// Let lapsed members go and close rounds whose time is up, every second,
    /// until the node stops.
    pub(crate) async fn sweep_until_stopped(self: Arc<Self>) {
        loop {
            tokio::select! {
                biased;
                () = self.stopping.cancelled() => return,
                () = tokio::time::sleep(SWEEP) => {}
            }
            let groups: Vec<Arc<Group>> = self
                .groups
                .iter()
                .map(|entry| Arc::clone(entry.value()))
                .collect();
            for group in groups {
                let changed = group.state.lock().await.sweep(Instant::now());
                if changed {
                    group.changed.notify_waiters();
                }
            }
        }
    }
}
