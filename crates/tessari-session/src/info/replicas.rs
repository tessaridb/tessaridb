//! How the cluster's members, reaches, failover policy and followers are described.

use crate::error::Result;
use std::collections::BTreeMap;
use tessari_storage::{Catalog, FollowerLag, Reach, ReplicaDefinition};
use tessari_types::{NamespaceId, Value};

/// One peer, as the catalog holds it.
///
/// An object rather than a bare endpoint, because a peer has a name an operator
/// wrote and an address they may change, and a list of addresses could not say
/// which one moved.
pub(crate) fn described_replica(
    replica: &ReplicaDefinition,
    catalog: &Catalog<'_, '_>,
) -> Result<Value> {
    Ok(Value::Object(BTreeMap::from([
        ("name".to_owned(), Value::from(replica.name.as_str())),
        (
            "endpoint".to_owned(),
            Value::from(replica.endpoint.as_str()),
        ),
        // Where a redirect sends a client (ADR-0101). `null` when unsaid, which
        // is the row whose redirects still name the peer door above.
        (
            "clients".to_owned(),
            replica.clients.as_deref().map_or(Value::Null, Value::from),
        ),
        (
            "http".to_owned(),
            replica.http.as_deref().map_or(Value::Null, Value::from),
        ),
        // Reported because it is now *routing*, not decoration: this is the
        // field that decides where a forwarded write lands, and a setting an
        // operator can write but cannot read back is one they cannot check
        // before the bad day. Named the same way `$node` names its own roles,
        // so the two sides of the membership row read alike.
        (
            "roles".to_owned(),
            Value::Array(replica.roles.names().into_iter().map(Value::from).collect()),
        ),
        // Reported for the same reason `roles` is, one step further: a binding
        // an operator can write and cannot read back is one they cannot check,
        // and the mistake it hides is the quiet one — a row bound to the wrong
        // id names a node that does not exist, so nothing converges and nothing
        // complains. Rendered as the id's own spelling, which is what `id` above
        // prints and what the `NODE` clause reads back.
        (
            "node".to_owned(),
            replica.node.map_or(Value::Null, Value::Uuid),
        ),
        // The third of three, and the one with the quietest failure: a peer
        // subscribed to nothing receives nothing, and a cluster in that state
        // reports no error anywhere — every node is up, every greeting lands,
        // and one copy simply never changes. Written back in the spelling the
        // clause takes, so what this prints can be pasted into the statement
        // that would correct it.
        (
            "replicates".to_owned(),
            match replica.replicates {
                None => Value::Null,
                Some(reach) => Value::from(spelled_reach(reach, catalog)?.as_str()),
            },
        ),
        // The placement (ADR-0082), in the spelling `LEADS` takes, so the answer
        // to *which node stands for which range* is on the row that decides it.
        (
            "leads".to_owned(),
            match replica.leads {
                None => Value::Null,
                Some(reach) => Value::from(spelled_reach(reach, catalog)?.as_str()),
            },
        ),
        // Whether that placement is being given back to the store line
        // (ADR-0098 D3): the range stays carved until the store's leader leads
        // it too and folds the placement away.
        ("releasing".to_owned(), Value::Bool(replica.releasing)),
        // The certificate allowed to bind this row (ADR-0108 D9), in the
        // spelling `FINGERPRINT` takes.
        (
            "fingerprint".to_owned(),
            replica
                .fingerprint
                .as_deref()
                .map_or(Value::Null, Value::from),
        ),
        // A join token waiting to bind it: when it stops binding, in
        // milliseconds since the Unix epoch, and never its digest — the digest
        // is what a token is checked against, and nothing reading this needs it.
        (
            "join_expires_ms".to_owned(),
            replica
                .join
                .as_ref()
                .map_or(Value::Null, |join| Value::from(join.expires_ms)),
        ),
    ])))
}

/// A subscription's reach, written the way the clause writes it.
///
/// Names and not ids: an id is a number the operator never typed and cannot act
/// on, and the whole reason to report a setting is that somebody can compare it
/// against what they meant. A name the catalog has lost is reported as the id it
/// could not resolve rather than omitted — a row pointing at a namespace that no
/// longer exists is precisely the state worth seeing.
pub(crate) fn spelled_reach(reach: Reach, catalog: &Catalog<'_, '_>) -> Result<String> {
    Ok(match reach {
        Reach::Store => "STORE".to_owned(),
        Reach::Namespace(namespace) => {
            format!("NAMESPACE {}", namespace_named(namespace, catalog)?)
        }
        Reach::Database(namespace, database) => {
            let held = catalog
                .databases_in(namespace)?
                .into_iter()
                .find(|found| found.id == database)
                .map_or_else(|| database.get().to_string(), |found| found.name);
            format!("DATABASE {}.{held}", namespace_named(namespace, catalog)?)
        }
        // `SHARD prod.shop.orders 2` — the clause's own spelling (G031), so the
        // report pastes back into the statement that would correct it.
        Reach::Shard(namespace, database, table, shard) => {
            let held = catalog
                .databases_in(namespace)?
                .into_iter()
                .find(|found| found.id == database)
                .map_or_else(|| database.get().to_string(), |found| found.name);
            let named = catalog
                .table(table)?
                .map_or_else(|| table.get().to_string(), |found| found.name);
            format!(
                "SHARD {}.{held}.{named} {}",
                namespace_named(namespace, catalog)?,
                shard.get()
            )
        }
    })
}

/// Each range's leader as the log recorded it (G053 C6), in range order.
///
/// From the `leaderships` rows the winner wrote under its own epoch — so this
/// is *as of epoch E, node N led range R*, a claim about the log and never about
/// who is alive now; the lease and the follower rows answer that.
pub(crate) fn described_leaders(catalog: &Catalog<'_, '_>) -> Result<Value> {
    let mut described = Vec::new();
    for held in catalog.leaderships()? {
        described.push(Value::Object(BTreeMap::from([
            (
                "range".to_owned(),
                Value::from(spelled_reach(held.range, catalog)?.as_str()),
            ),
            ("node".to_owned(), Value::Uuid(held.node)),
            (
                "epoch".to_owned(),
                Value::from(i64::try_from(held.epoch.get()).unwrap_or(i64::MAX)),
            ),
        ])));
    }
    Ok(Value::Array(described))
}

/// One namespace's name, or its id when the catalog no longer holds it.
pub(crate) fn namespace_named(namespace: NamespaceId, catalog: &Catalog<'_, '_>) -> Result<String> {
    Ok(catalog
        .namespace(namespace)?
        .map_or_else(|| namespace.get().to_string(), |found| found.name))
}

/// One follower, as the leader has experienced it.
///
/// Both units, because each has a blind spot the other covers. A follower that
/// stopped collecting while this leader was idle is behind by nothing at all —
/// `behind` reads zero and it looks well, because in sequences it *is* well;
/// only `quiet_for` grows. A follower collecting steadily but unable to keep up
/// has almost no `quiet_for`; only `behind` grows. That is why a primary
/// worth monitoring publishes positions and lags rather than either alone.
///
/// `quiet_for` is time since this follower last collected. `copy_age` is the
/// other question — how old the data it holds is — and the two come apart on an
/// idle leader, where a perfectly level follower's `quiet_for` grows for as long
/// as there is nothing to collect while its copy stays current. `copy_age` is
/// read against the leader's own timeline of its tail, so it is an upper bound
/// overstating by at most one sampling interval, and it is `null` when the copy
/// predates everything this leader has sampled — beyond every bound, not zero.
/// A failover policy as a report shows it: the five periods and the pair.
///
/// The periods are durations rather than numbers, because the store has a
/// duration type and an integer here would put the unit in a doc comment
/// somewhere else. The pair is beside them rather than inside them: `epoch` and
/// `version` are not periods, they are which policy this is, and a reader
/// comparing two nodes compares the pair first.
pub(crate) fn described_failover(held: &tessari_storage::FailoverDefinition) -> Value {
    let period = |span: std::time::Duration| {
        tessari_types::Duration::new(
            i64::try_from(span.as_secs()).unwrap_or(i64::MAX),
            span.subsec_nanos(),
        )
        .map_or(Value::Null, Value::Duration)
    };
    Value::Object(BTreeMap::from([
        ("awareness".to_owned(), period(held.policy.awareness())),
        ("collection".to_owned(), period(held.policy.collection())),
        ("round".to_owned(), period(held.policy.round())),
        ("campaign".to_owned(), period(held.policy.campaign())),
        ("lease".to_owned(), period(held.policy.lease())),
        (
            "epoch".to_owned(),
            Value::from(i64::try_from(held.epoch.get()).unwrap_or(i64::MAX)),
        ),
        (
            "version".to_owned(),
            Value::from(i64::try_from(held.version).unwrap_or(i64::MAX)),
        ),
        (
            "balance_leaderships".to_owned(),
            Value::Bool(held.balance_leaderships),
        ),
    ]))
}

pub(crate) fn described_follower(lag: FollowerLag) -> Value {
    Value::Object(BTreeMap::from([
        ("node".to_owned(), Value::Uuid(lag.node)),
        (
            "sequence".to_owned(),
            Value::Number(tessari_types::Number::Integer(
                i64::try_from(lag.sequence.get()).unwrap_or(i64::MAX),
            )),
        ),
        (
            "behind".to_owned(),
            Value::Number(tessari_types::Number::Integer(
                i64::try_from(lag.behind).unwrap_or(i64::MAX),
            )),
        ),
        (
            "quiet_for".to_owned(),
            tessari_types::Duration::new(
                i64::try_from(lag.quiet_for.as_secs()).unwrap_or(i64::MAX),
                lag.quiet_for.subsec_nanos(),
            )
            .map_or(Value::Null, Value::Duration),
        ),
        (
            "copy_age".to_owned(),
            lag.copy_age.map_or(Value::Null, |age| {
                tessari_types::Duration::new(
                    i64::try_from(age.as_secs()).unwrap_or(i64::MAX),
                    age.subsec_nanos(),
                )
                .map_or(Value::Null, Value::Duration)
            }),
        ),
    ]))
}
