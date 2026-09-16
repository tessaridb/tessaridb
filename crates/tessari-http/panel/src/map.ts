//! The cluster, drawn.
//!
//! The one signature element of this console, and the thing the owner asked for
//! by name. Everything here comes from `INFO FOR NODE` and nothing is inferred:
//! the fields, and the reasons each may or may not be drawn, are settled in the
//! data contract at `reports/2026-09-15-190000-g026-cluster-map-data-contract.md`,
//! which was itself derived by running a node rather than by reading anything.
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
import { show, type Subject } from "./drawer.js";

/** The three bits, in the order the engine defines them. */
const BITS: readonly { readonly name: string; readonly letter: string; readonly means: string }[] =
  [
    { name: "serving", letter: "S", means: "answers client requests" },
    { name: "writable", letter: "W", means: "accepts writes rather than forwarding them" },
    { name: "coordinating", letter: "C", means: "takes part in deciding, not only in storing" },
  ];

/** One peer, as `cluster.peers[]` carries it. */
export interface Peer {
  readonly name?: string;
  readonly endpoint?: string;
  readonly node?: string;
  readonly replicates?: string;
  readonly roles?: readonly string[];
}

/** What `INFO FOR NODE` answered, in the shape the map reads. */
export interface Seen {
  readonly id?: string;
  readonly roles?: readonly string[];
  readonly endpoints?: readonly string[];
  readonly cluster?: {
    readonly lease?: unknown;
    readonly epoch?: unknown;
    readonly campaigns?: unknown;
    readonly desired?: readonly string[] | null;
    readonly followers?: readonly unknown[];
    readonly peers?: readonly Peer[];
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
function lamps(has: readonly string[], wanted: readonly string[] | null): HTMLElement {
  const row = made("div", "lamps");
  for (const bit of BITS) {
    const held = has.includes(bit.name);
    const asked = wanted !== null && wanted.includes(bit.name);
    row.appendChild(lamp(bit.letter, held ? "held" : asked ? "wanted" : "off", bit.means));
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
  if (held === undefined || held === null) {
    return null;
  }
  const badge = made("div", "lease");
  const held_ = held as { until?: unknown; expires?: unknown };
  const until = told(held_.until ?? held_.expires ?? held);
  badge.textContent = until === null ? "holds the lease" : `holds the lease until ${until}`;
  return badge;
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

/** Draw the cluster as this node sees it. */
export function draw(into: HTMLElement, seen: Seen): void {
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
      fact("collecting from here", String((cluster.followers ?? []).length)),
      wanted === null ? null : fact("declared for it", wanted.join(", ")),
    ],
    "self",
    { name: "This node", self: true, endpoint: null, node: null, roles: mine, declared: wanted },
  );
  into.appendChild(self);

  for (const peer of cluster.peers ?? []) {
    into.appendChild(
      figure(
        peer.name ?? "(unnamed peer)",
        peer.roles ?? [],
        null,
        [
          fact("answers on", told(peer.endpoint)),
          fact("id", told(peer.node)),
          fact("replicates", told(peer.replicates)),
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
        },
      ),
    );
  }
}
