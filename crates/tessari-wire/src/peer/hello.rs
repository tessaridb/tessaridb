use super::*;

impl Hello {
    /// This node's own greeting, built from its own stored identity.
    ///
    /// Taken from [`NodeIdentity`] rather than assembled field by field so that
    /// a node cannot greet under an id, a role set or a build that disagree with
    /// what it actually holds. The two arguments are the two facts an identity
    /// deliberately does not carry, because both change with every commit.
    #[must_use]
    pub fn about(
        identity: &NodeIdentity,
        epoch: Epoch,
        tail: Sequence,
        tail_leadership: Epoch,
        current_as_of: Option<Duration>,
        policy: Option<FailoverStamp>,
    ) -> Self {
        Self {
            node: identity.id,
            build: identity.version,
            epoch,
            roles: identity.roles,
            tail,
            tail_leadership,
            current_as_of,
            policy,
            line: None,
        }
    }

    /// Where this greeter stands on `range`'s line, as the pair a voter orders
    /// two logs by — zero when the greeter's line is another range or none.
    ///
    /// Zero is the true position for a node never placed on the range: only a
    /// placed node leads it, so the log a node that never led it holds there is
    /// empty (ADR-0082).
    #[must_use]
    pub fn reached_on(&self, range: Reach) -> crate::grant::Reached {
        if range == Reach::Store {
            return self.reached();
        }
        self.line.filter(|line| line.range == range).map_or(
            crate::grant::Reached {
                leadership: Epoch::ZERO,
                tail: Sequence::ZERO,
            },
            |line| line.reached(),
        )
    }

    /// Where this greeter's log has got to, as the pair that orders two logs.
    ///
    /// The two fields travel together in every comparison, so they are paired
    /// here rather than at each call site — a caller that assembled them itself
    /// could pair [`Self::tail`] with [`Self::epoch`], which is the one mistake
    /// that inverts the answer.
    #[must_use]
    pub fn reached(&self) -> crate::grant::Reached {
        crate::grant::Reached {
            leadership: self.tail_leadership,
            tail: self.tail,
        }
    }

    /// The body of a [`PeerFrame::Hello`] frame.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut body = Vec::with_capacity(70);
        body.extend_from_slice(&self.node);
        frame::put_u32(&mut body, self.build.major);
        frame::put_u32(&mut body, self.build.minor);
        frame::put_u32(&mut body, self.build.patch);
        frame::put_u64(&mut body, self.epoch.get());
        body.push(self.roles.bits());
        frame::put_u64(&mut body, self.tail.get());
        frame::put_u64(&mut body, self.tail_leadership.get());
        // A presence byte and then the seconds, rather than a sentinel value:
        // every `u64` is a legitimate age, so there is no number left over to
        // mean *I cannot say*.
        match self.current_as_of {
            Some(age) => {
                body.push(1);
                frame::put_u64(&mut body, whole_seconds(age));
            }
            None => {
                body.push(0);
                frame::put_u64(&mut body, 0);
            }
        }
        // Appended after everything that came before it, and that position is
        // the compatibility rule rather than a habit: a peer built before this
        // field existed ends its body here, and a reader that takes the earlier
        // offsets first has already read every field such a peer can offer.
        match self.policy {
            Some(stamp) => {
                body.push(1);
                frame::put_u64(&mut body, stamp.epoch.get());
                frame::put_u64(&mut body, stamp.version);
            }
            None => {
                body.push(0);
                frame::put_u64(&mut body, 0);
                frame::put_u64(&mut body, 0);
            }
        }
        // ADR-0082. After everything, for the policy's reason, and written only
        // when there is one, so a node with no placement greets in the bytes it
        // always has.
        if let Some(line) = self.line {
            frame::put_reach(&mut body, line.range);
            frame::put_u64(&mut body, line.leading.get());
            frame::put_u64(&mut body, line.tail.get());
            frame::put_u64(&mut body, line.tail_leadership.get());
        }
        body
    }

    /// Read one back.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Malformed`] when the body is not the shape a greeting
    /// takes, and [`Error::UnknownRoles`] when it is the right shape but names a
    /// role this build does not have — which is a newer peer rather than a
    /// broken one, and is worth saying so.
    pub fn decode(body: &[u8]) -> Result<Self> {
        let mut node = [0_u8; NODE_ID_LEN];
        let head = body.get(..NODE_ID_LEN).ok_or(Error::Malformed)?;
        node.copy_from_slice(head);

        let (major, at) = frame::take_u32(body, NODE_ID_LEN)?;
        let (minor, at) = frame::take_u32(body, at)?;
        let (patch, at) = frame::take_u32(body, at)?;
        let (epoch, at) = frame::take_u64(body, at)?;
        let bits = *body.get(at).ok_or(Error::Malformed)?;
        let roles = Roles::from_bits(bits).ok_or(Error::UnknownRoles { bits })?;
        let (tail, at) = frame::take_u64(body, at.checked_add(1).ok_or(Error::Malformed)?)?;
        let (tail_leadership, at) = frame::take_u64(body, at)?;
        let present = *body.get(at).ok_or(Error::Malformed)?;
        let (seconds, at) = frame::take_u64(body, at.checked_add(1).ok_or(Error::Malformed)?)?;
        let policy = take_policy(body, at)?.map(|(epoch, version)| FailoverStamp {
            epoch: Epoch::new(epoch),
            version,
        });
        let line = take_line(body, at)?.map(|(range, leading, tail, written)| Line {
            range,
            leading: Epoch::new(leading),
            tail: Sequence::new(tail),
            tail_leadership: Epoch::new(written),
        });

        Ok(Self {
            node,
            build: NodeVersion {
                major,
                minor,
                patch,
            },
            epoch: Epoch::new(epoch),
            roles,
            tail: Sequence::new(tail),
            tail_leadership: Epoch::new(tail_leadership),
            current_as_of: (present != 0).then(|| Duration::from_secs(seconds)),
            policy,
            line,
        })
    }
}

/// The two numbers of the policy stamp, when the greeting reaches that far.
///
/// A body that **ends** at `at` is a peer built before the field existed, and
/// that is a node with nothing to say about a policy rather than a truncated
/// greeting — so it answers `Ok(None)` and never [`Error::Malformed`]. A body
/// that starts the field and then stops mid-way is a different thing entirely: a
/// real truncation, refused, because a greeting that half-arrived is not one a
/// reader may guess the rest of.
///
/// The numbers and not the [`FailoverStamp`], so that the epoch is rebuilt in
/// [`Hello::decode`] with every other decoded field. An epoch is the cluster's
/// count of leaderships and only the campaign may create one; the enforcement
/// test that holds that rule reads the name of the enclosing function, which is
/// what keeps the rule a name rather than a list of line numbers.
pub(super) fn take_policy(body: &[u8], at: usize) -> Result<Option<(u64, u64)>> {
    let Some(present) = body.get(at) else {
        return Ok(None);
    };
    let (epoch, at) = frame::take_u64(body, at.checked_add(1).ok_or(Error::Malformed)?)?;
    let (version, _) = frame::take_u64(body, at)?;
    Ok((*present != 0).then_some((epoch, version)))
}

/// The placed line, when the greeting reaches that far (ADR-0082).
///
/// `at` is where the policy stamp begins, which is a fixed seventeen bytes; a
/// body ending at or before the stamp's end carries no line, as every greeting
/// from a node with no placement does. Anything after it must read whole. The
/// numbers and not the epochs, for [`take_policy`]'s reason.
pub(super) fn take_line(body: &[u8], at: usize) -> Result<Option<(Reach, u64, u64, u64)>> {
    let after = at.saturating_add(17);
    if body.len() <= after {
        return Ok(None);
    }
    let (range, at) = frame::take_reach(body, after)?;
    let (leading, at) = frame::take_u64(body, at)?;
    let (tail, at) = frame::take_u64(body, at)?;
    let (written, _) = frame::take_u64(body, at)?;
    Ok(Some((range, leading, tail, written)))
}

/// An age in whole seconds, rounded up.
///
/// Up rather than down, and the direction is the point: a bound admits a copy
/// no older than it says, so reporting a fraction of a second as a whole one can
/// only put a copy *outside* a bound it was marginally inside. Refusing a read
/// that was borderline is recoverable; admitting one that was not is the thing
/// the bound exists to stop.
pub(super) fn whole_seconds(age: Duration) -> u64 {
    age.as_secs()
        .saturating_add(u64::from(age.subsec_nanos() > 0))
}
