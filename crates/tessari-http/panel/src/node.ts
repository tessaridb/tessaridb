//! This machine, and what the cluster tab can say about it today.
//!
//! The operational routes here are the ones any monitor already scrapes, so
//! nothing on this tab is a capability only the console has.

import { held, scrape, valueOf } from "./api.js";
import { at, clear, say } from "./dom.js";
import { facts } from "./draw.js";
import { draw as drawMap, type Seen } from "./map.js";
import { told } from "./session.js";
import { settled, state } from "./states.js";
import { onArrival } from "./tabs.js";

const HEALTH_ROUTE = "/health";
const READY_ROUTE = "/ready";
const METRICS_ROUTE = "/metrics";

/**
 * Prometheus text as the pairs it carries.
 *
 * Comment lines are the type and help, which a person reading a console does
 * not need beside the number. A line's value is what follows its last space,
 * because the name may carry labels and a label may carry a space.
 */
function readings(text: string): Record<string, number> {
  const out: Record<string, number> = {};
  for (const line of text.split("\n")) {
    if (line.startsWith("#") || line.trim() === "") {
      continue;
    }
    const cut = line.lastIndexOf(" ");
    if (cut > 0) {
      out[line.slice(0, cut)] = Number(line.slice(cut + 1));
    }
  }
  return out;
}

/**
 * One scraped route as a line.
 *
 * The status and the body say the same thing twice when both are well, and
 * different things when they are not — which is the case worth reading, so each
 * one is drawn as the status followed by what the node said.
 */
function answer(scraped: { status: number; body: unknown }): string {
  const body = scraped.body;
  const words =
    typeof body === "object" && body !== null
      ? Object.entries(body)
          .map(([name, value]) => name + " " + String(value))
          .join(", ")
      : String(body).trim();
  return scraped.status + " " + words;
}

export async function readNode(): Promise<void> {
  say("node-status", "asking…");
  try {
    const answered = held(await valueOf("INFO FOR NODE;", "Node"));
    const all = answered ?? {};
    // `cluster` is its own object and belongs on the cluster tab; what is left
    // is this machine, which is what this pane claims to show.
    const { cluster, ...mine } = all;
    facts("node-facts", mine);
    const peers =
      typeof cluster === "object" && cluster !== null
        ? (cluster as { peers?: unknown }).peers
        : undefined;
    // `membership` is gone from the answer and is not drawn from anywhere else.
    // It could only ever report `alone`, including on a node whose writes were
    // being fenced for belonging to a cluster — so a reader took a constant for
    // a claim, on the one screen where that claim mattered most. `roles` is what
    // answers the question it looked like it was answering, and it comes from
    // the authority the node actually holds.
    // The map first, then the answer it was read from.
    clear("cluster-map");
    drawMap(at("cluster-map"), all as Seen);
    facts("cluster-facts", {
      roles: all["roles"],
      peers: peers ?? [],
      endpoints: all["endpoints"],
      id: all["id"],
    });
    // A node with no peers drew one card and said nothing, so the map read the
    // same whether this node is alone or the map simply had nothing to add.
    // They are different situations with different next actions, and the four
    // states exist precisely so a screen cannot leave the reader to guess.
    if (!Array.isArray(peers) || peers.length === 0) {
      state(
        "cluster-status",
        "empty",
        "No peers — this node holds everything itself. Declare the membership " +
          "below to add them, all at once.",
      );
    } else {
      settled("cluster-status");
    }
  } catch (failure) {
    clear("node-facts");
    // The map goes with it. `node-facts` was already cleared here and the map
    // was not, so a failed read left a cluster drawn on screen under a line
    // saying the node could not be reached — the two halves of the pane
    // disagreeing about whether anything is known. A map nobody can vouch for
    // is worse than no map during the incident a map is for, so what stays is
    // the sentence.
    clear("cluster-map");
    clear("cluster-facts");
    say("node-status", told(failure), true);
    say("cluster-status", told(failure), true);
    return;
  }

  const [health, ready, metrics] = await Promise.all([
    scrape(HEALTH_ROUTE),
    scrape(READY_ROUTE),
    scrape(METRICS_ROUTE),
  ]);
  facts("node-health", { health: answer(health), ready: answer(ready) });
  facts(
    "node-metrics",
    typeof metrics.body === "string"
      ? readings(metrics.body)
      : (metrics.body as Record<string, unknown>),
  );
  say("node-status", "");
}

export function wire(): void {
  at("node-refresh").addEventListener("click", readNode);

  // Read once when either section is first REACHED, rather than on load: a
  // console left on the query tab should not be scraping a node nobody is
  // looking at.
  //
  // On arrival rather than on a click of the tab, which is what this listened
  // for until W319 measured it. ⌘2, the tablist's arrow keys, a link somebody
  // shared and a plain reload all reach the cluster map without clicking
  // anything, and each one used to leave it empty with an empty status beside
  // it — a blank map that an operator has no way to tell from a cluster with
  // nothing in it.
  // On EVERY arrival, not the first. A first arrival that was refused — because
  // nobody had signed in yet — used to spend the registration, and the map then
  // showed that refusal until the tab was closed.
  //
  // A declaration accepted by the node reaches this the same way: the screen
  // that is on view reads again, so the drawer and the formation form both say
  // `declared` beside a map that has been told.
  onArrival(["cluster", "this-node"], () => void readNode());
}
