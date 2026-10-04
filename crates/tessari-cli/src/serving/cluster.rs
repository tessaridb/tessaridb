use super::*;

/// Hand the store everything a node with peers reaches them through: the
/// gathering of split tables, the routing, the shared sign-in budget and the
/// carriage of requests and transactions to their leaders.
pub(super) fn join_the_cluster(db: std::sync::Arc<Db>, surface: &Peering) -> Result<(), String> {
    let me = db
        .store()
        .node_identity()
        .map_err(|why| format!("this node cannot say who it is: {why}"))?
        .id;
    let speaking = std::sync::Arc::downgrade(&db);
    let gathering = tessari_wire::Gathering::new(
        std::sync::Arc::clone(&db),
        me,
        surface.keys.clone(),
        std::sync::Arc::clone(&surface.routing),
        Box::new(move || {
            speaking
                .upgrade()
                .ok_or(tessari_wire::GreetingUnavailable::Stopping)
                .and_then(|db| greeting(&db).map_err(tessari_wire::GreetingUnavailable::Store))
        }),
    );
    db.gather_through(std::sync::Arc::new(gathering));
    // Every session on every surface knows who leads and where its peers
    // are, so a read HTTP cannot answer here names — or is carried to — the
    // node that can (Q-863). Spelled with the concrete type for the unsizing.
    db.among(std::sync::Arc::<tessari_wire::Published>::clone(
        &surface.routing,
    ));
    // And one sign-in budget for the whole cluster, held by the store
    // line's leader (ADR-0108 D5), so N nodes are not N allowances.
    let speaking = std::sync::Arc::downgrade(&db);
    db.budget_through(std::sync::Arc::new(tessari_wire::SharedBudget::new(
        me,
        surface.keys.clone(),
        std::sync::Arc::clone(&surface.routing),
        Box::new(move || {
            speaking
                .upgrade()
                .ok_or(tessari_wire::GreetingUnavailable::Stopping)
                .and_then(|db| greeting(&db).map_err(tessari_wire::GreetingUnavailable::Store))
        }),
    )));
    // And every request it cannot answer — a write another node leads, a
    // read another node holds — is carried there over the peer link, under
    // an assertion signed with this node's key, for a caller who cannot
    // follow a redirect (ADR-0108 D1–D3). No password crosses.
    let speaking = std::sync::Arc::downgrade(&db);
    let coordinator = std::sync::Arc::new(tessari_wire::Coordinator::new(
        std::sync::Arc::downgrade(&db),
        me,
        surface.keys.clone(),
        Box::new(move || {
            speaking
                .upgrade()
                .ok_or(tessari_wire::GreetingUnavailable::Stopping)
                .and_then(|db| greeting(&db).map_err(tessari_wire::GreetingUnavailable::Store))
        }),
    ));
    let carrying: std::sync::Arc<dyn tessaridb::Coordinate> =
        std::sync::Arc::<tessari_wire::Coordinator>::clone(&coordinator);
    db.coordinate_through(carrying);
    // The same carriage takes a transaction's records to the leaders of the
    // ranges it writes (ADR-0112).
    db.participating_through(coordinator);
    // And readers that meet an intent their copy cannot decide ask the
    // record's leader through it (ADR-0112 D13d).
    db.decide_reads_through_leaders();
    Ok(())
}
