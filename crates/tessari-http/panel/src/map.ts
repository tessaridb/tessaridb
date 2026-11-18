//! The cluster, drawn.
//!
//! The one signature element of this console, and the thing the owner asked for
//! by name. Everything here comes from `INFO FOR NODE` and nothing is inferred:
//! the fields, and the reasons each may or may not be drawn, are settled in the
//! data contract at `reports/2026-09-15-190000-g026-cluster-map-data-contract.md`,
//! which was itself derived by running a node rather than by reading anything.
//!
//! # The topology layer (G090 SG6)
//!
//! Over the same answer, and over two more named subjects, the map draws the
//! topology — the layer the contract at
//! `reports/2026-11-18-104500-g090-sg6-topology-data-contract.md` settles field
//! by field, also derived by running a node. Realms are group boxes from
//! `INFO FOR TOPOLOGY`, with the engine's own survival verdict labelled as its
//! verdict; members are placed by the region the member row carries — the
//! domain each member stands in is accepted and validated by the engine but
//! answers no INFO field, so domains render as the scheme's declared labels and
//! never as member placement. Cross-cluster links are labelled edges from
//! `INFO FOR REPLICATIONS`, the far side drawn as its endpoint (a link crosses
//! to another cluster, whose inside this node's catalog does not hold), and the
//! confirmed position rendered only where the registry holds one — process-local
//! truth, labelled as what it is, absent read as "no confirmation held" and
//! never as zero. Split tables are a glyph row from `cluster.balanced`, counted
//! on the measuring leader's last pass. None of it is inferred; each element
//! says which kind of truth it is, the way the peer lamps already do.
//!
//! # Three lamps, not one badge
//!
//! A role is three INDEPENDENT bits — serving, writable, coordinating — so there
//! are eight combinations and no taxonomy of *leader / follower / standby* to
//! draw. Three mutually exclusive badges would be inventing one, and would have
//! no way at all to show the state the engine calls out as the operator's own
//! drain mechanism: no roles at all, which is a node still holding its data and
//! answering nothing.
//!
//! Each lamp carries its LETTER. Meaning never rests on colour alone — not for a
//! reader who cannot separate two of them, and not on a projector at the back of
//! an incident room.
//!
//! # Leadership is a lease, not a role
//!
//! `coordinating` means the node may STAND FOR leadership. Whether it holds it
//! is `cluster.lease`, and the lease has an expiry. A leadership marker without
//! a clock implies a permanence the engine never promised — and the engine's own
//! source carries a note about an earlier version that read the role where it
//! should have read the lease.
//!
//! # A peer's lamps are DECLARED and say so
//!
//! This node knows what it declared about a peer. It has not asked the peer what
//! it reports, and there is no field that would answer. So a peer's lamps are
//! drawn as declarations and labelled as declarations; drawing them filled, like
//! this node's reported ones, would be the map claiming an observation nobody
//! made.

import { made } from "./dom.js";
import { resourceLines, resourceSummary, type Readings } from "./readings.js";
import { show, type Subject } from "./drawer.js";

/** The three bits, in the order the engine defines them. */
const BITS: readonly {
    readonly name: string;
    readonly letter: string;
    readonly means: string;
}[] = [
    { name: "serving", letter: "S", means: "answers client requests" },
    {
        name: "writable",
        letter: "W",
        means: "accepts writes rather than forwarding them",
    },
    {
        name: "coordinating",
        letter: "C",
        means: "takes part in deciding, not only in storing",
    },
];

/** One peer, as `cluster.peers[]` carries it. */
export interface Peer {
    readonly name?: string;
    readonly endpoint?: string;
    /** Where a client reaches it (`CLIENTS AT`), `null` when the row does not say. */
    readonly clients?: string | null;
    readonly node?: string;
    readonly replicates?: string;
    readonly leads?: string | null;
    /** The placement is being given back to the store line (ADR-0098 D3). */
    readonly releasing?: boolean;
    /** `LEADS … PREFERRED`: the candidate its range's leader hands it to. */
    readonly preferred?: boolean;
    /** The region a `LOCAL MAJORITY` counts it in, `null` when its row names none. */
    readonly region?: string | null;
    readonly roles?: readonly string[];
    /** The certificate pinned to the row (ADR-0108 D9), as `FINGERPRINT` spells it. */
    readonly fingerprint?: string | null;
    /** When a waiting join token stops binding, milliseconds since the epoch. */
    readonly join_expires_ms?: number | null;
    /**
     * What this node last heard of the peer's own resources — the peer's own
     * sampler's figures, joined here by the id the greeting proved, never
     * inferred. `null` when this node has not heard from it.
     */
    readonly readings?: Readings | null;
}

/** Where this node stands against the peer it collects from (ADR-0094 D4). */
interface Upstream {
    readonly state?: string;
    readonly copied_records?: unknown;
    readonly copies?: unknown;
}

/** One follower this node has served, as `cluster.followers` reports it. */
interface Follower {
    readonly node?: unknown;
    readonly behind?: unknown;
}

/** One range's leader as the log recorded it, as `cluster.leaders` reports it. */
interface Leader {
    readonly range?: unknown;
    readonly node?: unknown;
    readonly epoch?: unknown;
}

/** Transactions across leaders, as `cluster.across` reports them. */
interface Across {
    readonly committed?: unknown;
    readonly aborted?: unknown;
    readonly in_doubt?: unknown;
    readonly pending?: unknown;
    readonly with_intents?: unknown;
}

/** One balanced table as the balancing pass last counted it (ADR-0113 D4). */
interface Balanced {
    readonly table?: unknown;
    readonly shards?: readonly { readonly id?: unknown; readonly records?: unknown; readonly complete?: unknown }[];
    readonly last_act?: unknown;
}

/** One realm's survival arithmetic, as `INFO FOR TOPOLOGY` answers it. */
interface RealmLoss {
    readonly survived?: unknown;
    readonly voters_left?: unknown;
    readonly voters_needed?: unknown;
}

/** One member a realm's row carries, as `INFO FOR TOPOLOGY` renders it. */
interface RealmMember {
    readonly name?: unknown;
    readonly endpoint?: unknown;
    readonly node?: unknown;
    readonly region?: unknown;
}

/** One realm of the declared scheme, as `INFO FOR TOPOLOGY` answers it. */
interface Realm {
    readonly name?: unknown;
    readonly domains?: readonly unknown[];
    readonly loss?: RealmLoss;
    readonly members?: readonly RealmMember[];
}

/** The declared scheme, as `INFO FOR TOPOLOGY <name>` answers it. */
interface Scheme {
    readonly name?: unknown;
    readonly survives_realm_loss?: unknown;
    readonly satisfied?: unknown;
    readonly realms?: readonly Realm[];
}

/**
 * One cross-cluster link, as `INFO FOR REPLICATIONS` lists it, with the
 * confirmed positions `INFO FOR REPLICATION <name>` holds — `null` when the
 * registry answered none or could not be read, never an invented zero.
 */
export interface Link {
    readonly row: Record<string, unknown>;
    readonly confirmed: string | null;
}

/**
 * The topology layer's inputs, both derived by running a node (the SG6
 * contract): `topology` is the scheme answer, or `null` when the subject
 * answered `null` — no scheme declared — which the map says rather than
 * drawing an empty diagram that would read as a fault.
 */
export interface Layout {
    readonly topology?: Scheme | null;
    readonly links?: readonly Link[];
}

/** What `INFO FOR NODE` answered, in the shape the map reads. */
export interface Seen {
    readonly id?: string;
    readonly roles?: readonly string[];
    readonly endpoints?: readonly string[];
    readonly version?: string;
    readonly build?: string;
    readonly retain?: number | null;
    /** This machine's own resource readings — the local sampler's last pass. */
    readonly resources?: Readings | null;
    readonly cluster?: {
        readonly lease?: unknown;
        readonly epoch?: unknown;
        readonly campaigns?: unknown;
        readonly desired?: readonly string[] | null;
        readonly followers?: readonly Follower[];
        readonly leaders?: readonly Leader[];
        readonly upstream?: Upstream | null;
        readonly peers?: readonly Peer[];
        readonly across?: Across;
        readonly failover?: { readonly balance_leaderships?: unknown } | null;
        readonly balanced?: readonly Balanced[];
    };
}

type Lamp = "held" | "wanted" | "off";

/**
 * One role bit, as a lamp.
 *
 * `held` is reported by the node; `wanted` is declared for it and not yet
 * reported, which is a reconciliation in progress and the only honest way to
 * draw a failover while it is happening; `off` is neither.
 */
function lamp(letter: string, state: Lamp, title: string): HTMLElement {
    const one = made("span", "lamp " + state);
    one.textContent = letter;
    one.title = title;
    // The state in words as well as in the class, because a lamp is a graphic and
    // a screen reader is owed the same three answers a sighted reader gets.
    one.setAttribute(
        "aria-label",
        `${title} — ${state === "held" ? "held" : state === "wanted" ? "declared, not yet held" : "not held"}`,
    );
    return one;
}

/** The three lamps for one node, given what it reports and what is wanted of it. */
function lamps(
    has: readonly string[],
    wanted: readonly string[] | null,
): HTMLElement {
    const row = made("div", "lamps");
    for (const bit of BITS) {
        const held = has.includes(bit.name);
        const asked = wanted !== null && wanted.includes(bit.name);
        row.appendChild(
            lamp(
                bit.letter,
                held ? "held" : asked ? "wanted" : "off",
                bit.means,
            ),
        );
    }
    return row;
}

/** A labelled value, or nothing at all when there is nothing to say. */
function fact(label: string, said: string | null): HTMLElement | null {
    if (said === null) {
        return null;
    }
    const line = made("div", "fact");
    const name = made("span", "faint");
    name.textContent = label;
    const value = made("span", "fact-value");
    value.textContent = said;
    line.append(name, value);
    return line;
}

/** A string, or `null` when the node answered nothing — never an invented word. */
const told = (value: unknown): string | null =>
    value === undefined || value === null ? null : String(value);

/**
 * The lease, with its expiry.
 *
 * The engine answers `null` when this node holds none, and the map says nothing
 * rather than saying "no". An absent marker is the honest drawing of an absent
 * lease; a crossed-out one would be a claim about the cluster's leadership that
 * this node cannot make — it knows only about itself.
 */
function lease(held: unknown): HTMLElement | null {
    const said = leaseText(held);
    if (said === null) {
        return null;
    }
    const badge = made("div", "lease");
    badge.textContent = said;
    return badge;
}

/** The lease as a sentence, `null` when nobody granted one. */
function leaseText(held: unknown): string | null {
    if (held === undefined || held === null) {
        return null;
    }
    const held_ = held as { until?: unknown; expires?: unknown };
    const until = told(held_.until ?? held_.expires ?? held);
    return until === null ? "holds the lease" : `holds the lease until ${until}`;
}

/** Three unlit lamps is a state with a name, and the name belongs on screen. */
const drained = (has: readonly string[]): boolean => has.length === 0;

function figure(
    title: string,
    has: readonly string[],
    wanted: readonly string[] | null,
    facts: readonly (HTMLElement | null)[],
    kind: "self" | "peer",
    subject: Subject,
): HTMLElement {
    // A figure is a button, not a div with a click handler: the drawer is reached
    // by keyboard and named to a screen reader for the same reason every other
    // control on this page is, and the map is the one place where "it is just a
    // diagram" would have been the excuse for skipping it.
    const box = made("article", "node " + kind);
    box.tabIndex = 0;
    box.setAttribute("role", "button");
    box.setAttribute("aria-label", `${title} — open its drawer`);
    box.addEventListener("click", () => show(subject));
    box.addEventListener("keydown", (pressed) => {
        if (pressed.key === "Enter" || pressed.key === " ") {
            pressed.preventDefault();
            show(subject);
        }
    });
    const head = made("div", "node-head");
    const name = made("h3");
    name.textContent = title;
    head.append(name, lamps(has, wanted));
    box.appendChild(head);
    if (drained(has)) {
        const note = made("p", "note warn");
        note.textContent =
            kind === "self"
                ? "Drained — it holds its data and answers nothing."
                : "Declared with no roles — drained.";
        box.appendChild(note);
    }
    if (kind === "peer") {
        const note = made("p", "faint");
        note.textContent = "lamps as declared here; this node has not asked it";
        box.appendChild(note);
    }
    for (const one of facts) {
        if (one !== null) {
            box.appendChild(one);
        }
    }
    return box;
}

/** How far the furthest follower is behind, or `null` when none has collected. */
function furthest(followers: readonly Follower[]): string | null {
    let most: number | null = null;
    for (const one of followers) {
        if (
            typeof one.behind === "number" &&
            (most === null || one.behind > most)
        ) {
            most = one.behind;
        }
    }
    return most === null ? null : `${most} record(s)`;
}

/** One node id in the spelling every field compares by: no hyphens, lower case. */
function bare(node: unknown): string | null {
    return typeof node === "string" ? node.split("-").join("").toLowerCase() : null;
}

/**
 * The ranges the log says `node` leads, each with the epoch it was decided
 * under, or `null` when none — a claim about the log, never about who is alive.
 */
function leading(leaders: readonly Leader[], node: unknown): string | null {
    const mine = bare(node);
    if (mine === null) {
        return null;
    }
    const ranges = leaders
        .filter((one) => bare(one.node) === mine)
        .map((one) => `${told(one.range)} (epoch ${told(one.epoch)})`);
    return ranges.length === 0 ? null : ranges.join(", ");
}

/** Each follower and how many records it is short, or `null` when none has collected. */
function behindEach(followers: readonly Follower[]): string | null {
    const each = followers
        .filter((one) => typeof one.behind === "number")
        .map((one) => `${(bare(one.node) ?? "?").slice(0, 8)}… ${told(one.behind)} behind`);
    return each.length === 0 ? null : each.join(", ");
}

/** A peer's placement, with what the operator said about it. */
function placement(peer: Peer): string | null {
    const range = told(peer.leads);
    if (range === null) {
        return null;
    }
    const said = [range];
    if (peer.preferred === true) {
        said.push("preferred");
    }
    if (peer.releasing === true) {
        said.push("being given back to the store line");
    }
    return said.join(", ");
}

/**
 * The split tables, as a glyph row: one entry per balanced table, one pill per
 * shard with the records the last pass counted (`≥` where the count stopped at
 * the bound), and the last act in the balancer's own words — or `null` on a
 * node that counts nothing. Counted on the measuring leader, and labelled as
 * that kind of truth, never as an observation of the whole cluster.
 */
function glyphs(balanced: readonly Balanced[] | undefined): HTMLElement | null {
    const tables = balanced ?? [];
    if (tables.length === 0) {
        return null;
    }
    const row = made("div", "glyphs");
    const label = made("p", "glyphs-truth faint");
    label.textContent =
        "split tables, counted on the leader's last pass — not re-measured for this screen";
    row.appendChild(label);
    for (const one of tables) {
        const glyph = made("div", "glyph");
        const name = made("span", "glyph-name");
        name.textContent = told(one.table) ?? "?";
        glyph.appendChild(name);
        for (const shard of one.shards ?? []) {
            const pill = made("span", "glyph-shard");
            pill.textContent = `${shard.complete === false ? "≥" : ""}${told(shard.records) ?? "?"}`;
            pill.title = `shard ${told(shard.id) ?? "?"}`;
            glyph.appendChild(pill);
        }
        const act = told(one.last_act);
        if (act !== null) {
            const said = made("span", "glyph-act faint");
            said.textContent = act;
            glyph.appendChild(said);
        }
        row.appendChild(glyph);
    }
    return row;
}

/**
 * The declared topology, as realm group boxes.
 *
 * Placement is by what the member row carries — the realm's own `members`
 * array, whose rows say `region` — and the map goes no finer: the domain a
 * member stands in is accepted by the engine but answers no INFO field (the
 * SG6 contract names that gap), so domains render as the scheme's declared
 * labels, never as placement somebody inferred. The survival verdict is the
 * engine's arithmetic, drawn with its numbers and labelled as its verdict.
 */
function topology(scheme: Scheme, seen: Seen): HTMLElement {
    const layer = made("section", "topology");
    const head = made("h3", "topology-name");
    const called = told(scheme.name);
    head.textContent = called === null ? "The declared topology" : `Topology ${called}`;
    layer.appendChild(head);
    const verdict = made("p", "topology-verdict");
    const satisfied = scheme.satisfied === true;
    verdict.textContent =
        (scheme.survives_realm_loss === true
            ? "claims to survive the loss of any one realm — "
            : "") +
        (satisfied
            ? "the engine's verdict now: the claim holds against the membership as it stands"
            : `the engine's verdict now: not satisfied — the scheme reports the gap, it does not close it`);
    layer.appendChild(verdict);

    const boxes = made("div", "realms");
    const placed = new Set<string>();
    for (const realm of scheme.realms ?? []) {
        const box = made("div", "realm");
        const title = made("h4");
        title.textContent = told(realm.name) ?? "(unnamed realm)";
        box.appendChild(title);
        const domains = (realm.domains ?? []).map(told).filter((d): d is string => d !== null);
        if (domains.length > 0) {
            const tags = made("p", "realm-domains faint");
            tags.textContent = `domains: ${domains.join(", ")}`;
            box.appendChild(tags);
        }
        for (const member of realm.members ?? []) {
            const who = made("div", "realm-member");
            who.textContent = told(member.name) ?? told(member.endpoint) ?? "?";
            const where = told(member.region);
            if (where !== null) {
                who.title = `member, placed by its region '${where}'`;
            }
            box.appendChild(who);
            const id = bare(member.node);
            if (id !== null) {
                placed.add(id);
            }
        }
        const loss = realm.loss;
        if (loss !== undefined) {
            const line = made("p", "realm-loss faint");
            const said =
                loss.survived === true
                    ? `loss of this realm: still elects — ${told(loss.voters_left) ?? "?"} of ${told(loss.voters_needed) ?? "?"} voters left`
                    : "loss of this realm: the claim does not survive it";
            line.textContent = said;
            box.appendChild(line);
        }
        boxes.appendChild(box);
    }
    layer.appendChild(boxes);
    // Peers no realm claims sit outside every box, in a row beneath — declared
    // members of the cluster whose placement is the absence of a label, drawn
    // as that absence and not filed under a realm by guesswork.
    const loose = (seen.cluster?.peers ?? []).filter((peer) => {
        const id = bare(peer.node);
        return id === null || !placed.has(id);
    });
    if (loose.length > 0) {
        const row = made("div", "realm-loose");
        const tag = made("p", "faint");
        tag.textContent = "members no realm claims:";
        row.appendChild(tag);
        for (const peer of loose) {
            const who = made("div", "realm-member loose");
            who.textContent = peer.name ?? "(unnamed peer)";
            row.appendChild(who);
        }
        layer.appendChild(row);
    }
    return layer;
}

/**
 * One cross-cluster link, as a labelled edge.
 *
 * The far side renders as its endpoint — this node's catalog holds its own
 * rows and what the link registries report, not the other cluster's inside.
 * The confirmed position is process-local truth the registry holds or does
 * not; an absent reading renders as "no confirmation held", never as zero.
 */
function linkEdge(link: Link): HTMLElement {
    const row = link.row;
    const name = told(row["name"]) ?? "(unnamed link)";
    const direction = row["direction"] === "inbound" ? "receives" : "serves";
    const endpoint = told(row["endpoint"]);
    const edge = made("div", "link-edge");
    const head = made("span", "link-word");
    head.textContent = `${name} — this cluster ${direction}`;
    edge.appendChild(head);
    if (endpoint !== null) {
        const at = made("span", "link-endpoint");
        at.textContent = endpoint;
        edge.appendChild(at);
    }
    if (row["paused"] === true) {
        const held = made("span", "link-paused");
        held.textContent = "paused";
        edge.appendChild(held);
    }
    const said =
        link.confirmed ??
        "no confirmation held — the registry's reading lives in the shipping process";
    const reading = made("span", "link-confirmed faint");
    reading.textContent = said;
    edge.appendChild(reading);
    return edge;
}

/** How the transactions across leaders this node coordinated ended. */
function ended(across: Across | undefined): string | null {
    if (across === undefined) {
        return null;
    }
    return `${told(across.committed) ?? "?"} committed, ${told(across.aborted) ?? "?"} aborted, ${told(across.in_doubt) ?? "?"} in doubt`;
}

/** What copies of the leader's state installed, or `null` before the first. */
function copied(upstream: Upstream | null | undefined): string | null {
    if (typeof upstream?.copies !== "number" || upstream.copies === 0) {
        return null;
    }
    return `${told(upstream.copied_records)} record(s) in ${upstream.copies} cop${upstream.copies === 1 ? "y" : "ies"}`;
}

/** Draw the cluster as this node sees it, with the topology layer over it. */
export function draw(into: HTMLElement, seen: Seen, layout?: Layout): void {
    const cluster = seen.cluster ?? {};
    const mine = seen.roles ?? [];
    const wanted = cluster.desired ?? null;

    const self = figure(
        "This node",
        mine,
        wanted,
        [
            lease(cluster.lease),
            fact("id", told(seen.id)),
            fact("answers on", (seen.endpoints ?? []).join(", ") || null),
            fact("epoch", told(cluster.epoch)),
            fact("campaigns", told(cluster.campaigns)),
            fact(
                "collecting from here",
                String((cluster.followers ?? []).length),
            ),
            fact("furthest follower behind", furthest(cluster.followers ?? [])),
            fact("each follower", behindEach(cluster.followers ?? [])),
            fact("leads, as the log records", leading(cluster.leaders ?? [], seen.id)),
            // Absent on a node that follows nobody: `in sync` there would be a
            // state it has never been in.
            fact("sync with its upstream", cluster.upstream?.state ?? null),
            fact("copied from its upstream", copied(cluster.upstream)),
            fact("across leaders, coordinated here", ended(cluster.across)),
            // `null` until the node's settling pass has looked, so the map says
            // nothing rather than a zero nobody measured.
            fact("records still pending here", told(cluster.across?.pending)),
            fact("transactions holding intents here", told(cluster.across?.with_intents)),
            fact(
                "balances leaderships",
                cluster.failover?.balance_leaderships === true ? "yes, when one node leads two lines more" : null,
            ),
            wanted === null ? null : fact("declared for it", wanted.join(", ")),
            fact("resources", resourceSummary(seen.resources)),
        ],
        "self",
        {
            name: "This node",
            self: true,
            endpoint: null,
            node: null,
            roles: mine,
            declared: wanted,
            about: {
                id: told(seen.id),
                version: seen.version ?? null,
                build: seen.build ?? null,
                "answers on": (seen.endpoints ?? []).join(", ") || null,
                roles: mine.join(", ") || "none held",
                "declared for it": wanted?.join(", ") ?? null,
                retain: seen.retain === null ? "unbounded" : told(seen.retain),
                resources: resourceLines(seen.resources),
                lease: leaseText(cluster.lease),
                epoch: told(cluster.epoch),
                campaigns: told(cluster.campaigns),
                "collecting from here": String((cluster.followers ?? []).length),
                "each follower": behindEach(cluster.followers ?? []),
                "leads, as the log records": leading(cluster.leaders ?? [], seen.id),
                "sync with its upstream": cluster.upstream?.state ?? null,
                "copied from its upstream": copied(cluster.upstream),
                "across leaders": ended(cluster.across),
                "balances leaderships":
                    cluster.failover?.balance_leaderships === true
                        ? "yes, when one node leads two lines more"
                        : null,
            },
        },
    );
    into.appendChild(self);

    // The topology layer, over the same figures: realms as group boxes, links
    // as labelled edges, split tables as a glyph row. `topology: null` is the
    // subject's own answer when nobody has declared a scheme — said in words,
    // because an empty diagram would read as a fault the engine never named.
    // Undefined means the question was not put (the read was refused), which
    // is a different silence and draws nothing.
    if (layout?.topology !== undefined) {
        if (layout.topology === null) {
            const absent = made("p", "topology-absent faint");
            absent.textContent =
                "No topology scheme is declared on this cluster — realms are not named, so the map draws the cluster as it stands rather than an empty diagram that would read as a fault.";
            into.appendChild(absent);
        } else {
            into.appendChild(topology(layout.topology, seen));
        }
    }
    for (const link of layout?.links ?? []) {
        into.appendChild(linkEdge(link));
    }
    const split = glyphs(cluster.balanced);
    if (split !== null) {
        into.appendChild(split);
    }

    for (const peer of cluster.peers ?? []) {
        into.appendChild(
            figure(
                peer.name ?? "(unnamed peer)",
                peer.roles ?? [],
                null,
                [
                    fact("answers on", told(peer.endpoint)),
                    // A redirect and a forwarded write go here rather than to the peer
                    // door above, so a row that names it is worth showing (ADR-0101).
                    fact(
                        "clients reach it at",
                        typeof peer.clients === "string" ? peer.clients : null,
                    ),
                    fact("id", told(peer.node)),
                    fact("replicates", told(peer.replicates)),
                    fact("region", typeof peer.region === "string" ? peer.region : null),
                    fact("leads", placement(peer)),
                    fact("leads, as the log records", leading(cluster.leaders ?? [], peer.node)),
                    fact("resources, as heard here", resourceSummary(peer.readings)),
                ],
                "peer",
                {
                    name: peer.name ?? "",
                    self: false,
                    endpoint: peer.endpoint ?? null,
                    node: peer.node ?? null,
                    roles: peer.roles ?? [],
                    // A peer's declaration is not this node's to read, and the drawer
                    // offers it no drain to qualify.
                    declared: null,
                    about: {
                        name: peer.name ?? null,
                        "answers on": told(peer.endpoint),
                        "clients reach it at": typeof peer.clients === "string" ? peer.clients : null,
                        id: told(peer.node),
                        roles: (peer.roles ?? []).join(", ") || "none declared",
                        replicates: told(peer.replicates),
                        region: typeof peer.region === "string" ? peer.region : null,
                        leads: placement(peer),
                        "leads, as the log records": leading(cluster.leaders ?? [], peer.node),
                        fingerprint: peer.fingerprint ?? null,
                        "join token expires": told(peer.join_expires_ms),
                        // The peer's own figures, stated by the peer and heard
                        // here — labelled as that kind of truth, and the label
                        // rides the value rather than living in a heading.
                        "resources, as this node last heard them": resourceLines(peer.readings),
                    },
                },
            ),
        );
    }
}
