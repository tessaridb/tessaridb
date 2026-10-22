//! A member inside a generation: sync, heartbeat, commit, leave, and the
//! sweep that lets lapsed members go.

use tessari_constants::KAFKA_GROUP_MEMBER_BYTES;
use tokio::time::Instant;

use super::{Member, Phase, State};
use crate::api::membership::{Place, Synced};
use crate::surface::errors;

impl State {
    /// The member a place names, or the code that refuses it.
    fn member_at(&mut self, place: &Place<'_>) -> Result<&mut Member, i16> {
        let generation = self.generation;
        let member = self
            .members
            .iter_mut()
            .find(|member| member.id == place.member)
            .ok_or(errors::UNKNOWN_MEMBER_ID)?;
        if place.generation == generation {
            Ok(member)
        } else {
            Err(errors::ILLEGAL_GENERATION)
        }
    }

    fn synced_error(code: i16) -> Synced {
        Synced {
            error_code: code,
            ..Synced::default()
        }
    }

    /// Take a sync; `None` while the leader's assignments have not arrived.
    pub(crate) fn sync(
        &mut self,
        place: &Place<'_>,
        assignments: &[(&str, &[u8])],
        now: Instant,
    ) -> Option<Synced> {
        let phase = self.phase;
        let member = match self.member_at(place) {
            Ok(member) => member,
            Err(code) => return Some(Self::synced_error(code)),
        };
        member.seen = now;
        if phase == Phase::Preparing {
            return Some(Self::synced_error(errors::REBALANCE_IN_PROGRESS));
        }
        if phase == Phase::Completing && self.leader.as_deref() == Some(place.member) {
            if assignments
                .iter()
                .any(|(_, assignment)| assignment.len() > KAFKA_GROUP_MEMBER_BYTES)
            {
                return Some(Self::synced_error(errors::INVALID_REQUEST));
            }
            for member in &mut self.members {
                if let Some((_, assignment)) = assignments.iter().find(|(id, _)| *id == member.id) {
                    member.assignment = assignment.to_vec();
                }
            }
            self.phase = Phase::Stable;
        }
        self.synced(place)
    }

    /// A sync's answer once the group is assigned, `None` while it is not yet.
    pub(crate) fn synced(&mut self, place: &Place<'_>) -> Option<Synced> {
        let phase = self.phase;
        let (protocol_type, protocol) = (self.protocol_type.clone(), self.protocol.clone());
        let member = match self.member_at(place) {
            Ok(member) => member,
            Err(code) => return Some(Self::synced_error(code)),
        };
        match phase {
            Phase::Stable => Some(Synced {
                error_code: errors::NONE,
                protocol_type,
                protocol,
                assignment: member.assignment.clone(),
            }),
            Phase::Completing => None,
            Phase::Preparing | Phase::Empty => {
                Some(Self::synced_error(errors::REBALANCE_IN_PROGRESS))
            }
        }
    }

    /// Take a heartbeat.
    pub(crate) fn heartbeat(&mut self, place: &Place<'_>, now: Instant) -> i16 {
        let phase = self.phase;
        match self.member_at(place) {
            Ok(member) => {
                member.seen = now;
                if phase == Phase::Preparing {
                    errors::REBALANCE_IN_PROGRESS
                } else {
                    errors::NONE
                }
            }
            Err(code) => code,
        }
    }

    /// Whether a commit at `generation` from `member` is taken now.
    pub(crate) fn may_commit(&mut self, generation: i32, member: &str, now: Instant) -> i16 {
        if generation < 0 {
            return if self.members.is_empty() {
                errors::NONE
            } else {
                errors::UNKNOWN_MEMBER_ID
            };
        }
        let phase = self.phase;
        let place = Place {
            group: "",
            generation,
            member,
            instance: None,
        };
        match self.member_at(&place) {
            Ok(_) if phase == Phase::Completing => errors::REBALANCE_IN_PROGRESS,
            Ok(member) => {
                member.seen = now;
                errors::NONE
            }
            Err(code) => code,
        }
    }

    /// Take a member leaving.
    pub(crate) fn leave(&mut self, member: &str, now: Instant) -> i16 {
        let Some(at) = self.members.iter().position(|known| known.id == member) else {
            return errors::UNKNOWN_MEMBER_ID;
        };
        self.members.remove(at);
        self.after_loss(now);
        errors::NONE
    }

    /// What a group does once it lost members: rebalance the rest, or empty.
    fn after_loss(&mut self, now: Instant) {
        if self.members.is_empty() {
            if self.phase != Phase::Empty {
                self.generation = self.generation.checked_add(1).unwrap_or(1);
            }
            self.empty();
            return;
        }
        match self.phase {
            Phase::Stable | Phase::Completing => self.start_round(now),
            Phase::Preparing => {
                self.try_complete(now);
            }
            Phase::Empty => {}
        }
    }

    /// Let lapsed members and ids go, and complete a round whose time is up;
    /// answers whether anything changed.
    pub(crate) fn sweep(&mut self, now: Instant) -> bool {
        self.pending.retain(|(_, lapses)| *lapses > now);
        let mut changed = self.try_complete(now);
        let preparing = self.phase == Phase::Preparing;
        let before = self.members.len();
        // A member waiting in a join cannot heartbeat; the round's own
        // deadline is what lets it go.
        self.members.retain(|member| {
            (preparing && member.joined)
                || member
                    .seen
                    .checked_add(member.session)
                    .is_none_or(|due| due > now)
        });
        if self.members.len() != before {
            self.after_loss(now);
            changed = true;
        }
        changed
    }
}
