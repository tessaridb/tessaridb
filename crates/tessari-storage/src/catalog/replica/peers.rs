use super::*;

/// Does the catalog name a peer that is not this node?
///
/// The one spelling of *this store is in a cluster*, and it lives here rather
/// than beside either of its callers because it is a question about
/// [`ReplicaDefinition`] and nothing else. `tessari_wire` re-exports it; a
/// second copy over there would be a second definition of membership, and the
/// two would agree until the day they did not.
///
/// # It is not "is the catalog empty"
///
/// [`Catalog::replicas`] returns every membership row, **including the one that
/// describes THIS node** — and the row a cluster writes to admit a newcomer is
/// exactly that row. So a joiner's first collection brings in one row, its own,
/// and an emptiness bound reads that as *the catalog can answer* and stops
/// dialling the seed, while `upstream` and the greeting round both skip the row
/// naming this node. The node collects once, follows nobody afterwards, and
/// nothing is in an error state while it happens (W260).
///
/// A row naming no node at all counts for nothing here, for the same reason it
/// counts for nothing upstream: there is no identity to dial, and none to check
/// a credential against.
///
/// # Why the write gate asks this and not what role the node was given
///
/// [`crate::Store::awaiting_leadership`] used to read `Roles::COORDINATING`, on
/// the reasoning that [`Roles::ALONE`] is documented as *not `COORDINATING`,
/// because there is nothing to coordinate with*, so the bit is already the line
/// between a member of a deciding set and a store on its own.
///
/// That is true about the line it draws and it answers the wrong question. The
/// roles are a **set**, not an enum, so `SERVING | WRITABLE` without
/// `COORDINATING` is a legal and ordinary declaration — a writable node that
/// does not vote. Such a node, sitting in a cluster beside an elected leader,
/// carried no lease and was asked for none: the gate wanted a bit it does not
/// have, so it wrote freely and silently, which is the failure the fence exists
/// to prevent arriving through the one role combination the predicate missed.
///
/// *May this node take part in deciding* and *could there be a leader other
/// than me* are two questions. The first is about the role. The second is about
/// the catalog, and this is it.
#[must_use]
pub fn names_a_peer(declared: &[ReplicaDefinition], me: &[u8; NODE_ID_LEN]) -> bool {
    declared
        .iter()
        .any(|peer| peer.node.is_some_and(|node| node != *me))
}

/// Could a node other than this one accept a write?
///
/// The question the write fence actually asks, and it is **not**
/// [`names_a_peer`]. That one answers *is this store in a cluster*, which its
/// two other callers need — a joiner deciding whether its catalog can yet name
/// somebody to follow counts a read-only peer, and should.
///
/// A peer that carries neither [`Roles::WRITABLE`] nor [`Roles::COORDINATING`]
/// can do neither thing this fence exists to guard against. It cannot be
/// elected, because `driver::voters` will not ballot a row without
/// `COORDINATING` and a majority counted over members that cannot be asked is a
/// majority of a fiction; and it cannot write under somebody else's leadership,
/// because the row declares that it takes no writes.
///
/// # What asking the wider question cost
///
/// A store whose only declared peer was a read-only follower was fenced against
/// a leadership its own election machinery refuses to create, with no
/// configuration that recovers it: a lease is written only by a completed round,
/// no round is possible, and the store never writes again. That is the topology
/// ADR-0067 documents — one node that is already a cluster, and a second told
/// one address — so following the join procedure stopped the leader's writes on
/// its first statement (Q-609).
///
/// # Why it is not narrowed to `COORDINATING` alone
///
/// Because that is the hole ADR-0069 closed, arriving by a different road. Two
/// nodes declared `SERVING | WRITABLE` and neither `COORDINATING` can elect
/// nobody, so neither would be fenced — and both would write. Under the wider
/// reading they fence each other, which is a dead cluster an operator can see
/// rather than a silent divergence. **Unable to write** is the property, and it
/// takes both bits to be absent.
///
/// # A row that understates its peer
///
/// `roles` is what an operator declared, not what the peer has since become. A
/// row that understates a peer defeats this, and it defeats the election and the
/// forwarding lookup in exactly the same breath — they all read the same field,
/// so the deciding set is what the catalog says it is, and one wrong row is one
/// wrong answer rather than two that disagree.
#[must_use]
pub fn another_node_may_write(declared: &[ReplicaDefinition], me: &[u8; NODE_ID_LEN]) -> bool {
    declared.iter().any(|peer| {
        peer.node.is_some_and(|node| node != *me)
            && (peer.roles.has(Roles::WRITABLE) || peer.roles.has(Roles::COORDINATING))
    })
}

/// Which declared row, if any, a greeting binds itself to (ADR-0108 D9).
///
/// A row with no `node` is **declared but undiallable**: `Directory::greet_round`
/// skips it, so the only event that can bind it is that peer arriving here and
/// proving who it is. The greeting supplies the **id and nothing else** — the
/// endpoint, the roles and the reach are what the operator wrote.
///
/// # Approved, never first-come
///
/// A row binds a greeter only when the operator said which one: its pinned
/// certificate [`ReplicaDefinition::fingerprint`] is the one presented, or a join
/// token the row is waiting on — unexpired at `now_ms` — is the one carried.
/// A row that says neither binds nobody. It used to bind whichever peer holding
/// a cluster-issued certificate greeted first, and that peer then received the
/// row's whole reach, users' credential hashes included (R-15).
///
/// Nothing is bound when a row **already names** the node — one node in the
/// catalog twice is two rows that can disagree about it — nor when the evidence
/// matches more than one row, which would be a guess. Whether the node was
/// dropped before (a tombstone) is the caller's question, asked of the catalog.
#[must_use]
pub fn the_row_a_greeting_binds<'a>(
    declared: &'a [ReplicaDefinition],
    greeter: &Greeter<'_>,
    now_ms: i64,
) -> Option<&'a str> {
    if declared.iter().any(|row| row.node == Some(greeter.node)) {
        return None;
    }
    let approves = |row: &&ReplicaDefinition| {
        row.node.is_none()
            && (row.fingerprint.as_deref() == Some(greeter.fingerprint)
                || row.join.as_ref().is_some_and(|join| {
                    Some(join.digest.as_str()) == greeter.token && join.expires_ms > now_ms
                }))
    };
    let mut approved = declared.iter().filter(approves);
    let row = approved.next()?;
    if approved.next().is_some() {
        return None;
    }
    Some(row.name.as_str())
}
