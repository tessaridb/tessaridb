//! The leadership round: standing for the lease a write rests on, and renewing it.

use crate::greeting_round::greeting;
use tessaridb::Db;

/// Stand for the leadership this node is declared to hold, once per campaign
/// interval.
///
/// # What decides whether this node stands at all
///
/// [`tessari_wire::voters`] does, from the roles this node's catalog **declares**
/// and the peers it declares coordinating. The declared roles and not the
/// effective ones: the effective set drops `WRITABLE` the moment the lease is
/// spent, so a leader that lost one round would stop campaigning and could never
/// stand again — a permanent demotion that looks exactly like a correct one.
///
/// It is asked every round rather than once, so a node the operator makes
/// writable begins defending a lease without a restart, and one the operator
/// stands down stops asking for epochs at the next tick.
///
/// # Why a node that holds no leadership starts from a spent lease
///
/// [`tessari_wire::Renewing`] holds a `Leadership` and asks whether its fence is
/// near enough to need standing again. A node that holds none is, arithmetically,
/// a node holding one that has already run out — so the cursor starts one whole
/// lease in the past and the first pass stands immediately, rather than teaching
/// the driver a second state that means the same thing.
///
/// # What a lost round does, and does not do
///
/// Nothing here. `Renewing` keeps the lease it holds when a round wins nothing,
/// because a round wins nothing in the ordinary case too — a cadence that fired
/// while there was still margin asked nobody at all. Standing down on that would
/// make a healthy leader resign on a timer. What ends a leadership is the fence,
/// which the store closes on its own.
pub(crate) async fn stand_for_leadership(
    db: std::sync::Arc<Db>,
    keys: tessari_wire::PeerKeys,
    voter: std::sync::Arc<tessari_wire::Deciding>,
    published: std::sync::Arc<tessari_wire::Published>,
    stop: tokio_util::sync::CancellationToken,
) {
    let started = std::time::Instant::now();
    // The placed range's line, with its own cursor (ADR-0082). `None` until the
    // first tick finds a placement, and replaced when the placement names a
    // different range — a cursor carries one line's epochs and no other's.
    let mut on_a_line: Option<OnALine> = None;
    // When this node last handed its range to a preferred candidate (G053 SG5b).
    let mut yielded: Option<Yielded> = None;
    let mut renewing = store_line_cursor(&db, started).await;
    // The campaign cadence the installed failover policy states, re-read every
    // pass (G053 SG2c): a pass that returns before it reaches the catalog keeps
    // the last one it read.
    let mut cadence = tessari_storage::Failover::DEFAULT.campaign();
    let mut pass = async move |now: std::time::Instant, cadence: &mut std::time::Duration| {
        let (db, keys, voter, published) = (&*db, &keys, &*voter, &*published);
        let store = db.store();
        // Identity first, and the catalog only once this node is known to
        // stand. Reading the roles costs a record; reading every replica the
        // catalog declares costs a transaction and a scan, and a node the
        // operator never made writable would otherwise pay for that scan
        // once a tick, for the life of the process, to reach a `return`.
        let me = match store.node_identity().await {
            Ok(identity) => identity,
            Err(why) => {
                tracing::warn!(error = %why, "this node cannot say who it is");
                return None;
            }
        };
        if !tessari_wire::stands(me.roles) {
            return None;
        }
        // Both in one transaction: the membership and the policy stamp are
        // read on every tick of this cadence, and a node the operator made
        // writable should pay for one begin here rather than two.
        let (declared, policy) = match crate::peers::declared_with_policy(store).await {
            Ok(read) => read,
            Err(why) => {
                tracing::warn!(error = %why, "this node cannot say who its peers are");
                return None;
            }
        };
        // The periods this cluster runs under: the policy the log carries,
        // or the build's when nobody has set one (G053 SG2c, Q-878).
        let periods = policy
            .as_ref()
            .map_or(tessari_storage::Failover::DEFAULT, |definition| {
                definition.policy
            });
        *cadence = periods.campaign();
        voter.hold_for(periods.lease());
        let voting = tessari_wire::voters(me.roles, &declared, &me.id)?;
        // ADR-0082. The placed range on its own line, beside the store's:
        // the store line's own guards return early — a follower that hears
        // the store leader does not stand for the STORE — and a range
        // this node is placed on must still be stood for while somebody else
        // leads the store.
        //
        // At once rather than one after the other (Q-946). Both lines are due
        // together, because one pass won both and dated both from its start;
        // run in turn, the second canvass opened a whole canvass after the
        // instant it was decided on — 210 ms at a 50 ms round trip, with
        // 13 ms of its lease left — and landed after its fence every time.
        let candidate = Candidate {
            me: me.id,
            declared: &declared,
            voting: &voting,
            keys,
            voter,
            published,
            periods,
            leads_the_store: store
                .leads(tessari_types::Reach::Store)
                .await
                .unwrap_or(false),
            roles: me.roles,
        };
        let (on_a_line, yielded) = (&mut on_a_line, &mut yielded);
        tokio::join!(
            stand_for_a_placed_range(db, &candidate, (on_a_line, yielded), now),
            stand_for_the_store(db, &candidate, (&mut renewing, policy), now),
        );
        let staggered = [
            renewing.not_before(),
            on_a_line.as_ref().and_then(|(_, line)| line.not_before()),
        ];
        due_to_stand(&candidate, staggered, now, *cadence)
    };
    // Woken early only at the instant this node becomes due to stand (Q-970):
    // otherwise it runs on the policy's cadence alone.
    let woken = std::sync::Arc::new(tokio::sync::Notify::new());
    let waking = std::sync::Arc::clone(&woken);
    tessari_wire::every_paced("leadership", &stop, &woken, cadence, async move |now| {
        if let Some(due) = pass(now, &mut cadence).await {
            let waking = std::sync::Arc::clone(&waking);
            drop(tokio::spawn(async move {
                tokio::time::sleep_until(tokio::time::Instant::from_std(due)).await;
                waking.notify_one();
            }));
        }
        Ok(cadence)
    })
    .await;
}

/// The instant inside the next `cadence` at which this node becomes due to
/// stand on either line it campaigns on — the store's or its placed range's —
/// when there is one (Q-970): it stops hearing a leader there, or the stagger
/// after a round it lost on that line ends.
///
/// # Why the cadence is woken for it
///
/// A node asks whether it hears a leader only on this cadence's ticks, and
/// every node started together ticks together. Left to the tick,
/// [`tessari_wire::election_timeout`]'s spread is rounded to one: two
/// survivors whose spreads fall in the same tick stand on it, each grants the
/// other the epoch before its own self-vote, both lose, and both wait out a
/// lease to collide again — the first failover after a cluster started took
/// 1.8-3.8 s where every later one took about one. Woken at the instant, a node
/// stands on its own spread, which is what the spread was for. The stagger
/// after a lost round is a spread of the same kind and is woken on the same way.
fn due_to_stand(
    candidate: &Candidate<'_>,
    staggered: [Option<std::time::Instant>; 2],
    now: std::time::Instant,
    cadence: std::time::Duration,
) -> Option<std::time::Instant> {
    let within = standing_window(candidate);
    let heard = candidate.published.current();
    let store = tessari_wire::heard_a_leader_until(
        candidate.declared,
        &heard,
        candidate.voter.granted_elsewhere_at(candidate.me),
        within,
    );
    let placed =
        tessari_wire::campaign_line(candidate.declared, &candidate.me, candidate.leads_the_store)
            .and_then(|range| {
                tessari_wire::heard_a_leader_on_until(
                    range,
                    candidate.me,
                    candidate.declared,
                    &heard,
                    candidate.voter.granted_elsewhere_on(range, candidate.me),
                    within,
                )
            });
    soonest_in_the_tick([store, placed].into_iter().chain(staggered), now, cadence)
}

/// The soonest of `instants` that falls after `now` and before the next tick,
/// one millisecond past it: a leader heard exactly `within` ago is still heard,
/// and a stagger that ends at an instant has not ended before it. An instant
/// the regular tick reaches first is left to the tick.
fn soonest_in_the_tick(
    instants: impl IntoIterator<Item = Option<std::time::Instant>>,
    now: std::time::Instant,
    cadence: std::time::Duration,
) -> Option<std::time::Instant> {
    let next_tick = now.checked_add(cadence)?;
    instants
        .into_iter()
        .flatten()
        .filter_map(|at| at.checked_add(std::time::Duration::from_millis(1)))
        .filter(|due| *due > now && *due < next_tick)
        .min()
}

/// How long this node goes without hearing a leader before it stands: the
/// lease plus its own spread (G053 SG2b), for the grant it last decided.
fn standing_window(candidate: &Candidate<'_>) -> std::time::Duration {
    tessari_wire::election_timeout(
        candidate.me,
        candidate
            .voter
            .decided()
            .unwrap_or(tessari_types::Epoch::ZERO),
        candidate.periods.lease(),
    )
}

/// Stand for the store line, if this node is the one that should (ADR-0066).
async fn stand_for_the_store(
    db: &Db,
    candidate: &Candidate<'_>,
    (renewing, policy): (
        &mut tessari_wire::Renewing,
        Option<tessari_storage::FailoverDefinition>,
    ),
    now: std::time::Instant,
) {
    let Candidate {
        me,
        declared,
        voting,
        keys,
        voter,
        published,
        periods,
        ..
    } = *candidate;
    // Q-857. The store line's leader must hold every table it writes,
    // so a node subscribed to less does not stand for it — after its
    // placed range, which it still leads.
    if !tessari_wire::stands_for_the_store(declared, &me) {
        return;
    }
    // ADR-0066. A node that can still hear a leader does not stand
    // against it — and this is not politeness, it is what stops a
    // follower's own self-vote from refusing that leader's renewal for a
    // whole lease. The bound is the lease term, because a greeting older
    // than the leader's lease cannot testify that the leader still holds
    // it. A node that hears nothing stands, which is the condition an
    // election exists for.
    //
    // `granted_elsewhere_at(me)` and not the grant instant alone: a
    // candidate self-votes through this same memory, so a node reading
    // its own vote here would be silenced by the act of standing — and
    // a leader renews by standing. That is Q-602, and it made a lease
    // un-renewable.
    //
    // The window is the lease plus this node's own spread (G053 SG2b):
    // every voter hears the same renewal, so without a spread their
    // memories of a dead leader lapse together and two of them stand on
    // one tick, grant each other the epoch and both lose it.
    if tessari_wire::heard_a_leader(
        declared,
        &published.current(),
        voter.granted_elsewhere_at(me),
        now,
        standing_window(candidate),
    ) {
        return;
    }
    // And the second thing a node can hear that means it should not
    // stand: a peer running a failover policy that supersedes this
    // node's own. The periods decide when a leader counts as gone, so a
    // candidate timing itself by a policy the cluster has already
    // replaced is the disagreement the policy row exists to remove,
    // arriving at the one moment where it decides an outcome.
    //
    // The bound is the staleness floor — one awareness interval to hear
    // and one more to notice it did not — because the question is
    // whether such a peer is still AUDIBLE, and greetings arrive once a
    // second, which is longer than the lease (G053 SG2b). And the
    // refusal lasts only while such a peer is audible — a cluster
    // cannot deadlock behind a node that has gone away, because a node
    // that has gone away advertises nothing.
    //
    // It is inert until somebody sets a policy: with no row anywhere,
    // every stamp is `None` and nothing supersedes anything.
    if let Some(newer) = tessari_wire::heard_a_newer_policy(
        declared,
        &published.current(),
        policy.map(|definition| definition.stamp()),
        now,
        periods.staleness_floor(),
    ) {
        tracing::info!(
            epoch = newer.epoch.get(),
            version = newer.version,
            "not standing: a peer runs a failover policy that supersedes this node's own"
        );
        return;
    }
    // Each member's `AT` as declared, an address or a host name, resolved
    // where its ballot is dialled (G075 C1). A member that cannot be reached
    // — its name included — stays in the set and gives no vote, so the
    // majority is counted over the membership the operator declared.
    let peers: Vec<_> = voting
        .iter()
        .map(|(node, endpoint)| (*node, endpoint.clone()))
        .collect();
    if peers.is_empty() {
        return;
    }
    let said = match greeting(db).await {
        Ok(said) => said,
        Err(why) => {
            tracing::warn!(error = %why, "this node cannot say what it holds");
            return;
        }
    };
    // Counted here, where the decision to stand has actually been
    // taken: every gate above has passed and a round is about to open.
    // Counting at the top of the cadence would count ticks, and the
    // cadence ticks every second whether or not anything happens —
    // which is precisely the difference this counter exists to show.
    db.store().campaigned();
    let standing = tessari_wire::Standing {
        candidate: me,
        keys,
        said: &said,
        peers: &peers,
        round: periods.round(),
        range: tessari_types::Reach::Store,
        lease: periods.lease(),
    };
    let before = renewing.standing();
    let held = match renewing.due(now) {
        Some((lease, next)) => {
            let stood = standing.renew(voter, lease, next, now).await;
            renewing.answered(me, now, next, stood)
        }
        None => renewing.standing(),
    };
    if held != before {
        // Installed as it was granted, whole. The lease is dated from the
        // instant the round opened, so handing the store a span instead
        // would restart that clock here and spend the canvass out of the
        // voters' window rather than this node's.
        db.hold(held.epoch, held.lease()).await;
    }
    if held.epoch != before.epoch {
        tracing::info!(epoch = held.epoch.get(), "leading the store line");
        // Written when the EPOCH changes and not when the lease does: a
        // renewal keeps its epoch and only moves the lease, about every
        // 300 ms for as long as this node keeps leading, and a row per
        // renewal would put a log record on the wire three times a
        // second forever — one every follower then pays to apply, on a
        // log that would never quiesce.
        //
        // Logged rather than propagated. The round already granted the
        // leadership and `hold` already installed it; this records that
        // grant in the log so a partitioned node can still answer who
        // leads. A store that refuses the write has not un-elected this
        // node, and treating it as fatal would let a disk hiccup
        // overturn a decision a majority took.
        if let Err(refused) = db
            .record_leadership(tessaridb::Reach::Store, held.epoch)
            .await
        {
            tracing::warn!(
                epoch = held.epoch.get(),
                refusal = %refused,
                "leading the store line but could not record it"
            );
        }
    }
}

/// Everything one campaign tick knows about who is standing and to whom.
///
/// A struct for the reason [`tessari_wire::Standing`] is one: the placed-range
/// campaign needs the store line's whole context, and nine positional arguments
/// is past what the linter and a reader accept.
pub(crate) struct Candidate<'a> {
    me: [u8; tessari_storage::NODE_ID_LEN],
    declared: &'a [tessari_storage::ReplicaDefinition],
    voting: &'a [([u8; tessari_storage::NODE_ID_LEN], String)],
    keys: &'a tessari_wire::PeerKeys,
    voter: &'a tessari_wire::Deciding,
    published: &'a tessari_wire::Published,
    /// The periods the installed failover policy states (G053 SG2c).
    periods: tessari_storage::Failover,
    /// Whether this node leads the store line, which makes it the candidate
    /// for a range being given back to it (ADR-0098 D3).
    leads_the_store: bool,
    /// The roles this node adopted, which decide whether it stands for a
    /// placed range at all (DB-16).
    roles: tessari_storage::Roles,
}

/// The placed line this node stands on, with its own cursor.
type OnALine = (tessari_types::Reach, tessari_wire::Renewing);

/// The range this node last handed to its preferred candidate, and when.
type Yielded = (tessari_types::Reach, std::time::Instant);

/// Stand for the one placed range this node campaigns on, on that range's own
/// line (ADR-0082): its own placement, or a range given back to the store line
/// it leads (ADR-0098 D3) — unless it is handing the range to a preferred
/// candidate (G053 SG5b).
///
/// The store line's cadence and its rules, applied to one range: ADR-0066's
/// *a node that hears a leader does not stand*, read per line; the margin rule
/// inside [`tessari_wire::Standing::renew`]; and a win recorded as the range's
/// leadership row, which homes at the range and so commits under the lease just
/// won. Logged rather than propagated, for the store line's reason: a round a
/// majority granted is not overturned by a store that refused the record of it.
pub(crate) async fn stand_for_a_placed_range(
    db: &Db,
    candidate: &Candidate<'_>,
    (on_a_line, yielded): (&mut Option<OnALine>, &mut Option<Yielded>),
    now: std::time::Instant,
) {
    if !tessari_wire::stands_for_a_placed_line(candidate.roles) {
        *on_a_line = None;
        return;
    }
    let Some(range) =
        tessari_wire::campaign_line(candidate.declared, &candidate.me, candidate.leads_the_store)
    else {
        *on_a_line = None;
        return;
    };
    if tessari_wire::heard_a_leader_on(
        range,
        candidate.me,
        candidate.declared,
        &candidate.published.current(),
        candidate.voter.granted_elsewhere_on(range, candidate.me),
        now,
        standing_window(candidate),
    ) {
        return;
    }
    if hands_to_the_preferred(db, candidate, range, yielded, now).await {
        return;
    }
    let peers: Vec<_> = candidate
        .voting
        .iter()
        .map(|(node, endpoint)| (*node, endpoint.clone()))
        .collect();
    if peers.is_empty() {
        return;
    }
    // The ballot names where this node stands on THIS range's line: a store
    // leader taking a range back is placed nowhere, so its greeting names no
    // line, and a voter would read it as holding none of the range.
    let said = match (async {
        let mut said = greeting(db).await?;
        said.line = Some(crate::greeting_round::line_of(db.store(), range).await?);
        Ok::<_, tessari_storage::Error>(said)
    })
    .await
    {
        Ok(said) => said,
        Err(why) => {
            tracing::warn!(error = %why, "this node cannot say what it holds");
            return;
        }
    };
    if on_a_line.as_ref().is_none_or(|(held, _)| *held != range) {
        // Above every leadership the range's logs already hold (DB-15): a
        // range placed after the store line wrote to it keeps those records,
        // and a follower reads a line numbered below them as deposed.
        let written = match db.store().newest_leadership_in(range).await {
            Ok(written) => written,
            Err(why) => {
                tracing::warn!(range = ?range, error = %why, "this node cannot say which leadership the range's logs reach");
                return;
            }
        };
        *on_a_line = Some((
            range,
            tessari_wire::Renewing::holding(tessari_wire::Leadership {
                length: tessari_storage::LEASE_TTL,
                epoch: tessari_types::Epoch::ZERO,
                from: now.checked_sub(tessari_storage::LEASE_TTL).unwrap_or(now),
            })
            .above(written),
        ));
    }
    let Some((_, renewing)) = on_a_line.as_mut() else {
        return;
    };
    let standing = tessari_wire::Standing {
        candidate: candidate.me,
        keys: candidate.keys,
        said: &said,
        peers: &peers,
        round: candidate.periods.round(),
        range,
        lease: candidate.periods.lease(),
    };
    let before = renewing.standing();
    let held = match renewing.due(now) {
        Some((lease, next)) => {
            let stood = standing.renew(candidate.voter, lease, next, now).await;
            renewing.answered(candidate.me, now, next, stood)
        }
        None => renewing.standing(),
    };
    if held != before {
        db.store().hold_range(range, held.epoch, held.lease()).await;
    }
    if held.epoch != before.epoch {
        tracing::info!(range = ?range, epoch = held.epoch.get(), "leading a range");
        if let Err(refused) = db.record_leadership(range, held.epoch).await {
            tracing::warn!(
                range = ?range,
                epoch = held.epoch.get(),
                refusal = %refused,
                "leading a range but could not record it"
            );
        }
    }
}

/// Whether this node stays off `range`'s line this tick because it handed the
/// range to its preferred candidate (G053 SG5b) — now, or recently enough that
/// the candidate is still being elected.
///
/// A leader that is not the range's preferred candidate, hearing one that is
/// caught up, stops renewing: its lease runs out and the preferred candidate is
/// elected, the hand-over a moved placement already takes (ADR-0098 D2). It
/// then stays off the line for four leases, so it does not win the range back
/// in the gap, and yields at most once per `PREFERENCE_YIELD_SECONDS`.
async fn hands_to_the_preferred(
    db: &Db,
    candidate: &Candidate<'_>,
    range: tessari_types::Reach,
    yielded: &mut Option<Yielded>,
    now: std::time::Instant,
) -> bool {
    let lease = candidate.periods.lease();
    let since = yielded
        .filter(|(on, _)| *on == range)
        .map(|(_, at)| now.saturating_duration_since(at));
    if since.is_some_and(|since| since < lease.saturating_mul(4)) {
        return true;
    }
    let patient = std::time::Duration::from_secs(tessari_constants::PREFERENCE_YIELD_SECONDS);
    if since.is_some_and(|since| since < patient) || db.store().leading_of(range).is_none() {
        return false;
    }
    let Ok(line) = crate::greeting_round::line_of(db.store(), range).await else {
        return false;
    };
    let Some(to) = tessari_wire::preferred_to_yield_to(
        range,
        candidate.me,
        candidate.declared,
        &candidate.published.current(),
        line.reached(),
        (now, candidate.periods.staleness_floor()),
    ) else {
        return false;
    };
    tracing::info!(
        range = ?range,
        to = %crate::greeting_round::hex(&to),
        "handing a range to its preferred candidate"
    );
    *yielded = Some((range, now));
    true
}

/// The store line's cursor at start, standing above every leadership the
/// line's logs already hold — as a placed range's does (DB-15).
///
/// A voter's memory of what it granted lives in the process, so a whole cluster
/// restarted together grants from nothing. A candidate counting from nothing
/// too wins an epoch below the one its own log was written under: a leadership
/// the catalog refuses to record, and no node writes again (Q-ENG-D3). A log
/// that cannot be read stands from nothing, as every node did before, and says
/// so.
async fn store_line_cursor(db: &Db, started: std::time::Instant) -> tessari_wire::Renewing {
    let written = match db
        .store()
        .newest_leadership_in(tessari_types::Reach::Store)
        .await
    {
        Ok(written) => written,
        Err(why) => {
            tracing::warn!(error = %why, "this node cannot say which leadership the store line's logs reach");
            tessari_types::Epoch::ZERO
        }
    };
    tessari_wire::Renewing::holding(tessari_wire::Leadership {
        length: tessari_storage::LEASE_TTL,
        epoch: tessari_types::Epoch::ZERO,
        // A lease taken a whole TTL ago is spent, so the first pass stands. If
        // the subtraction cannot be represented — a process started before the
        // clock had a lease's worth of history behind it — the node waits out
        // one lease before its first round, which is the safe direction.
        from: started
            .checked_sub(tessari_storage::LEASE_TTL)
            .unwrap_or(started),
    })
    .above(written)
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use tessari_session::Now;
    use tessari_storage::{LEASE_TTL, Lease};
    use tessari_types::Epoch;
    use tessaridb::Db;

    use super::{soonest_in_the_tick, store_line_cursor};

    #[test]
    fn a_node_restarted_over_a_written_log_stands_above_the_leadership_it_holds() {
        // A whole cluster restarted: every voter forgot what it granted, and a
        // candidate counting from nothing wins an epoch below the one its own
        // log was written under — a leadership the catalog can never record,
        // so no node writes again (Q-ENG-D3).
        let db = Db::in_memory().now().expect("an in-memory store");
        db.store()
            .hold(Epoch::new(190), Lease::taken(LEASE_TTL))
            .now();
        tessari_session::RunNow::run_now(&mut db.session(), "DEFINE NAMESPACE led")
            .expect("a write under leadership 190");
        let started = Instant::now();
        let cursor = store_line_cursor(&db, started).now();
        let (_, next) = cursor.due(started).expect("a spent lease stands");
        assert_eq!(next, Epoch::new(191));
    }

    const TICK: Duration = Duration::from_millis(100);

    #[test]
    fn the_soonest_instant_inside_the_tick_is_woken_on_a_millisecond_past_it() {
        let now = Instant::now();
        let late = now + Duration::from_millis(70);
        let early = now + Duration::from_millis(20);
        assert_eq!(
            soonest_in_the_tick([Some(late), Some(early)], now, TICK),
            Some(early + Duration::from_millis(1))
        );
    }

    #[test]
    fn an_instant_the_tick_reaches_first_or_one_already_past_wakes_nothing() {
        let now = Instant::now();
        let past = now - Duration::from_millis(5);
        let beyond = now + TICK;
        assert_eq!(
            soonest_in_the_tick([Some(past), Some(beyond)], now, TICK),
            None
        );
        assert_eq!(soonest_in_the_tick([None, None], now, TICK), None);
        // The control: the same instants with one inside the tick do wake.
        let inside = now + Duration::from_millis(50);
        assert_eq!(
            soonest_in_the_tick([Some(past), Some(inside), Some(beyond)], now, TICK),
            Some(inside + Duration::from_millis(1))
        );
    }
}
