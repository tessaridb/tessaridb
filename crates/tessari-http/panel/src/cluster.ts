//! Cluster — what this node knows about the cluster it belongs to.
//!
//! Extracted from `page.ts` for the reason `access.ts` was. It is the screen S4
//! rebuilds into a map, so it is the one most worth having on its own already.

import { el, type Node } from "./html.js";
import { to } from "./destinations.js";
import { answer, note, pane, paneHead, panel, status, warning } from "./ui.js";

export const cluster = (): Node =>
  panel(
    to("cluster"),
    false,
    pane(paneHead("Membership", status("cluster-status")), answer("cluster-facts", "small")),
    pane(
      paneHead("What is here, and what is not"),
      note(
        "What the pane above shows is what this node itself knows: its own " +
          "roles, the peers it has been told about, and the addresses it answers " +
          "on. It is read from the node, not from anything standing beside it.",
      ),
      note(
        "Membership is a record and not a control plane: a node learns its peers " +
          "through the same log it replicates data with, so there is nothing to stand " +
          "up beside the database. A namespace declares how many copies the cluster " +
          "keeps, and a peer's subscription decides which of them it receives. A " +
          "leader holds a renewable lease granted by a majority and stops writing " +
          "before that lease expires, measured on elapsed time rather than on the " +
          "clock. A read may name the staleness it will accept, and a bound tighter " +
          "than the cluster can know about itself is refused with the floor named. A " +
          "write sent to the wrong node comes back as a redirect carrying the " +
          "endpoint, the node and the epoch it was decided under. A divergence is " +
          "refused at the first divergent record and counted, never silently ranked.",
      ),
      warning(
        el("strong", {}, "There is no sharding."),
        " Every node that holds a namespace holds all of it; one dataset is not " +
          "split across machines by key. A namespace larger than one machine is the " +
          "case this engine does not serve.",
      ),
      note(
        el("strong", {}, "This pane is not yet a cluster surface."),
        " Nothing here manages anything: roles, failover, standby nodes, data " +
          "placement and a live map of cluster state are being designed, and are " +
          "deliberately absent rather than stubbed. Nor does it draw lag or " +
          "leadership for other nodes — nothing pulls a replica forward on a timer, " +
          "so a node that is not writing has no last collection its copy could be " +
          "measured from, and a number invented here would be the dashboard drawn " +
          "ahead of the engine that makes the rest of this console untrustworthy.",
      ),
    ),
  );
