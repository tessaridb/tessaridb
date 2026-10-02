//! The greeting round: what this node learns about its peers, and what it tells them.

use tessaridb::Db;

/// Greet every peer the catalog declares, once per awareness interval.
///
/// # Why this cadence and not a number chosen here
///
/// `tessari_session` refuses a read whose staleness bound is tighter than
/// `STALENESS_FLOOR_SECONDS`, and that floor is derived from
/// `AWARENESS_SECONDS`. The refusal is only honest if this node actually learns
/// every peer's age that often, so the period is read from the same constant the
/// floor is derived from. Two copies of it would let the promise the API makes
/// and the mechanism behind it drift apart with nothing failing.
///
/// # Why the catalog is re-read every round
///
/// A node joining the cluster is a new catalog row, and the round after it
/// appears is the one that should dial it. Reading the declarations once at
/// start would mean a node that joined had to wait for every existing node to be
/// restarted before anyone greeted it.
///
/// # What a failure does, and does not do
///
/// A peer that will not answer is left exactly as it was: its last reading stays
/// and goes on ageing, which drifts it out of tighter bounds first and looser
/// ones later, and restores it the moment it answers again. Erasing it instead
/// would put it outside *every* bound at once, so one dropped packet would take
/// a healthy node out of all routing. A round in which nobody answered is a
/// cluster in trouble rather than an operation that went wrong, so it is logged
/// and the cadence runs again.
pub(crate) async fn dial_peers(
    db: std::sync::Arc<Db>,
    mine: tessari_wire::Credential,
    authority: tessari_wire::CertificateDer<'static>,
    seeds: Vec<tessari_wire::Seed>,
    published: std::sync::Arc<tessari_wire::Published>,
    wakes: std::sync::Arc<crate::peers::Wakes>,
    stop: tokio_util::sync::CancellationToken,
) {
    // The periods the installed failover policy states, re-read every pass
    // (G053 SG2c): a pass that returns before it reaches the catalog keeps the
    // last ones it read.
    let mut periods = tessari_storage::Failover::DEFAULT;
    let woken = std::sync::Arc::clone(&wakes);
    // The leader the last round pointed this node at, so the collection round is
    // woken when that changes and not on every greeting.
    let mut followed: Option<[u8; tessari_storage::NODE_ID_LEN]> = None;
    tessari_wire::every_paced(&stop, &woken.greeting, move |now| {
        let (db, mine, authority, seeds, published) =
            (&*db, &mine, &authority, &seeds[..], &*published);
        // Read through the pieces the facade already publishes rather than
        // through a new `Db` method: `Db::store` and `Store::begin` are both
        // public, so a `Db::declared_peers` would be a second name for a
        // capability this binary can already reach — which is the finding
        // W238 recorded when it wrote and then reverted `Db::holding`.
        let store = db.store();
        // Date this node's own tail on the same cadence, because the copy
        // age this makes measurable is only honest at the interval the
        // staleness floor is derived from. It rides this round rather than
        // the commit path deliberately: see `Store::mark_tail`.
        if let Err(why) = store.mark_tail(tessari_types::Reach::Store) {
            log::warn!("this node cannot date its own log position: {why}");
        }
        let declared = store.begin().and_then(|mut transaction| {
            let catalog = tessari_storage::Catalog::new(&mut transaction);
            Ok((catalog.replicas()?, catalog.failover()?))
        });
        let (me, declared) = match (store.node_identity(), declared) {
            (Ok(identity), Ok((declared, policy))) => {
                periods = policy.map_or(tessari_storage::Failover::DEFAULT, |definition| {
                    definition.policy
                });
                (identity.id, declared)
            }
            (Err(why), _) => {
                log::warn!("this node cannot say who it is: {why}");
                return periods.awareness();
            }
            (_, Err(why)) => {
                log::warn!("this node cannot say who its peers are: {why}");
                return periods.awareness();
            }
        };
        let mut reached = 0_usize;
        published.round(|directory| {
            // The seeds INSTEAD of the catalog, and only while the catalog
            // names no peer BUT THIS NODE. A node that has just been told
            // to join holds no replica rows, so `greet_round` would dial
            // nobody and this node would never learn anything; once
            // collection brings a row naming somebody else in, the catalog
            // is the answer and a seed still being dialled would be a
            // second source of truth about who the members are — see
            // `Directory::greet_seeds`. The *but this node* is load-bearing
            // and was `is_empty` until W260: the row a cluster writes to
            // admit a newcomer describes the NEWCOMER, so the joiner's
            // first collection left it holding one row, its own, which
            // answers nothing and stopped the seed all the same.
            let greet = |endpoint: &str, node| {
                tessari_wire::call(
                    endpoint,
                    mine.duplicate(),
                    authority,
                    node,
                    &greeting(db).map_err(|why| why.to_string())?,
                    tessari_wire::Ask::Nothing,
                )
                .map(|(said, _)| said)
                .map_err(|why| {
                    // Said here rather than swallowed. The rounds keep only
                    // a count, so without this the one line an operator
                    // gets for a directory that has stopped refreshing is
                    // *nobody answered* — and a directory that stops
                    // refreshing is a follower that stops knowing who to
                    // follow, whose symptom is a copy that silently never
                    // changes.
                    log::warn!("the greeting to {endpoint} did not land: {why}");
                    why.to_string()
                })
            };
            reached = if tessari_wire::names_a_peer(&declared, &me) {
                directory.greet_round(&declared, &me, now, greet)
            } else {
                directory.greet_seeds(seeds, &me, now, greet)
            };
        });
        // The count and not the directory, because nothing reads the
        // directory yet — routing on it is S6.2 and is a wave of its own.
        // What this round makes observable today is that the dialling
        // happens at all and how much of the cluster answered.
        let (kind, dialled) = if tessari_wire::names_a_peer(&declared, &me) {
            ("declared peer", declared.len())
        } else {
            ("seed", seeds.len())
        };
        if reached == 0 && dialled > 0 {
            log::warn!("no {kind} answered this round; {dialled} were dialled");
        } else {
            log::info!("{reached} of {dialled} {kind}(s) answered");
        }
        // G053 SG2b. A clustered node that may not write and can name no
        // leader to follow greets again after one round time rather than
        // one awareness interval: that is the window right after a leader
        // died, and a directory a second old is what kept a follower away
        // from its new leader for up to a whole interval. Once a leader is
        // heard the cadence relaxes again, so a cluster without a leader
        // for a long time costs five greetings a second, not more.
        let roles = store.effective_roles().ok();
        let leading = roles.is_some_and(|roles| roles.has(tessari_storage::Roles::WRITABLE));
        let leader = roles.and_then(|roles| {
            tessari_wire::upstream(roles, &declared, &published.current()).map(|(node, _)| node)
        });
        if leader != followed {
            followed = leader;
            wakes.collection.notify_one();
        }
        if tessari_wire::names_a_peer(&declared, &me) && !leading && leader.is_none() {
            periods.round()
        } else {
            periods.awareness()
        }
    })
    .await;
}

/// Bind the row this greeting is evidence for, when there is exactly one.
///
/// # Why this is here and not inside the door
///
/// `Peers::greet` has no store, deliberately — it settles a credential, hears a
/// greeting and answers, and a door that could also write the catalog would be a
/// transport with an opinion about membership. The rule needs two things the door
/// cannot both see, so it lives in the caller that holds both, which is the shape
/// W281 arrived at for the write fence for the same reason.
///
/// # Why a refusal is not an error here
///
/// Binding is a catalog write and therefore a log record, so only a node that may
/// write can take it: on a follower the fence refuses the commit, which is
/// correct, because membership arrives at a follower by collection and a
/// follower writing its own would be a second source of truth about who the
/// members are. The read runs first and commits nothing, so in the ordinary case
/// — every row already bound — this costs one catalog read and writes nothing at
/// all.
///
/// Logged at the level the loop already uses for ordinary peer outcomes. A
/// greeting that arrived and a row that did not need binding are both the normal
/// course of a running cluster.
pub(crate) fn bind_the_greeter(db: &Db, node: [u8; tessari_storage::NODE_ID_LEN]) {
    let bind = || -> Result<Option<String>, String> {
        let mut transaction = db.store().begin().map_err(|why| why.to_string())?;
        let mut catalog = tessari_storage::Catalog::new(&mut transaction);
        let declared = catalog.replicas().map_err(|why| why.to_string())?;
        let Some(name) = tessari_storage::the_row_a_greeting_binds(&declared, &node) else {
            return Ok(None);
        };
        let name = name.to_owned();
        catalog
            .bind_replica_node(&name, node)
            .map_err(|why| why.to_string())?;
        transaction.commit().map_err(|why| why.to_string())?;
        Ok(Some(name))
    };
    match bind() {
        Ok(Some(name)) => log::info!("peer {} now names replica {name}", hex(&node)),
        Ok(None) => {}
        Err(why) => log::info!("peer {} was not bound to a declared row: {why}", hex(&node)),
    }
}

/// What this node would tell a peer about itself, right now.
///
/// Built through [`tessari_wire::Hello::about`] rather than field by field, so
/// that a node cannot greet under an id, a role set or a build that disagree
/// with what its own store holds.
pub(crate) fn greeting(db: &Db) -> Result<tessari_wire::Hello, tessari_storage::Error> {
    let store = db.store();
    let identity = store.node_identity()?;
    // Where this node stands on the store's line: the history the line shares,
    // not the few records it once wrote alone (ADR-0107, Q-879).
    let own = store.history_log(tessari_types::Reach::Store)?;
    let tail = store.committed_tail(own)?;
    let current_as_of = store.current_as_of()?;
    // The leadership this node is actually writing under — and the trigger the
    // previous version of this line named has now fired.
    //
    // It used to be the constant `Epoch::ZERO`, which was true rather than lazy:
    // no epoch was ever allocated in this path, so every record the node held
    // belonged to the first and only leadership. A campaign now runs here, so
    // the constant would be a node telling every peer it leads under an epoch it
    // does not — and `Hello::epoch` is documented as *the leadership it believes
    // is current*, which is a claim peers route on.
    //
    // Read from the store rather than from the newest log record, which was the
    // other candidate: the record read costs a `Reach::Store` scan inside a
    // process that answers a network, and it answers a different question — what
    // leadership WROTE the last thing here, not what leadership this node holds
    // now. A follower holds records written under epochs it never led.
    //
    // `None` becomes `Epoch::ZERO`, so a node that never campaigns greets
    // byte-identically to every build before this one.
    let leading = store.leading().unwrap_or(tessari_types::Epoch::ZERO);
    // And the other epoch, which is a different fact: the leadership that WROTE
    // what this node holds, rather than the one it holds a lease under. A voter
    // ranks candidates on this pair, and ranking on `leading` instead would put
    // a follower carrying the newest records below an ex-leader carrying fewer.
    let tail_leadership = store.tail_leadership(own)?;
    // Which failover policy this node is running under, read from its own
    // catalog rather than assembled from the constants it currently times by.
    // The two are not the same claim: the constants are what this build compiled
    // with, and the stamp is what the cluster last agreed on — and a node
    // advertising the first while holding the second would be telling its peers
    // it is level when it is behind, which is the one thing this field exists to
    // make visible.
    //
    // `None` is the ordinary state today, because nothing sets the row yet.
    let (policy, declared) = store.begin().and_then(|mut transaction| {
        let catalog = tessari_storage::Catalog::new(&mut transaction);
        Ok((catalog.failover()?, catalog.replicas()?))
    })?;
    let policy = policy.map(|definition| definition.stamp());
    let mut said = tessari_wire::Hello::about(
        &identity,
        leading,
        tail,
        tail_leadership,
        current_as_of,
        policy,
    );
    // ADR-0082. The one placed range this node stands for, and where it stands
    // there: a voter judges a ballot on that range by this, and a candidate
    // hears a live leader of the range by it. Read in the same transaction as
    // the policy, so a greeting is one reading of the catalog.
    if let Some(range) = tessari_wire::stands_for(&declared, &identity.id) {
        let log = store.history_log(range)?;
        said.line = Some(tessari_wire::Line {
            range,
            leading: store
                .leading_of(range)
                .unwrap_or(tessari_types::Epoch::ZERO),
            tail: store.committed_tail(log)?,
            tail_leadership: store.tail_leadership(log)?,
        });
    }
    Ok(said)
}

/// A node id as it is written in a log line.
pub(crate) fn hex(id: &[u8]) -> String {
    id.iter().map(|byte| format!("{byte:02x}")).collect()
}
