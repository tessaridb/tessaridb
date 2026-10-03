//! Cluster — what this node knows about the cluster it belongs to.
//!
//! Extracted from `page.ts` for the reason `access.ts` was. It is the screen S4
//! rebuilds into a map, so it is the one most worth having on its own already.

import { el, type Node } from "./html.js";
import { to } from "./destinations.js";
import { trustPanes } from "./trust-panes.js";
import {
    answer,
    behindDisclosure,
    button,
    field,
    note,
    pane,
    paneHead,
    panel,
    row,
    says,
    status,
    text,
} from "./ui.js";

export const cluster = (): Node =>
    panel(
        to("cluster"),
        false,
        pane(
            paneHead(
                "The cluster, as this node sees it",
                status("cluster-status"),
            ),
            // The map, and the raw answer under it. Both, because the map is a
            // reading of the answer and an operator who doubts the reading needs the
            // thing it was read from — that is the same rule the statement log
            // follows for statements.
            el("div", { id: "cluster-map", class: "map" }),
            // The drawer sits inside the pane and over the map, so opening it never
            // costs the view the decision is being made against.
            el(
                "div",
                { id: "drawer", class: "sheet drawer", hidden: true },
                row(
                    "spread",
                    el("h3", { id: "drawer-title" }, "a node"),
                    button("drawer-close", "Close", "quiet"),
                ),
                el("div", { id: "drawer-missing" }),
                row(
                    "default",
                    ...["serving", "writable", "coordinating"].map((bit) =>
                        field(
                            bit,
                            el("input", {
                                id: `drawer-${bit}`,
                                type: "checkbox",
                            }),
                            { class: "tick" },
                        ),
                    ),
                ),
                says("drawer-says"),
                row(
                    "default",
                    button("drawer-apply", "Declare it", "primary"),
                    status("drawer-status"),
                ),
                el(
                    "div",
                    { id: "drawer-remove-part" },
                    row(
                        "default",
                        field("Type its name to remove it", text("drawer-remove-confirm")),
                        field("Why", text("drawer-remove-why", { placeholder: "machine retired" })),
                    ),
                    says("drawer-remove-says"),
                    row(
                        "default",
                        button("drawer-remove", "Remove from the cluster", "default", { disabled: true }),
                        status("drawer-remove-status"),
                    ),
                ),
                note(
                    // The drain used to be named here as a thing with no control. It has
                    // one now, so the note keeps only the half still true and says nothing
                    // about draining — the radius line does that, where the operator is
                    // standing when it matters, and a 40-word note has no room for both.
                    // ALTER REPLICA is a store-line write, so it succeeds only on the
                    // store's leader; a control here would compose a statement that fails
                    // on every other node, so the note names it instead.
                    "Handing the store's leadership over has no statement yet, so no control " +
                        "is offered for it. A placed range moves with ALTER REPLICA <peer> LEADS " +
                        "<range> or LEADS NONE, run on the store's leader from the query tab.",
                ),
            ),
            behindDisclosure(
                "The answer this was drawn from",
                answer("cluster-facts", "small"),
            ),
        ),
        pane(
            paneHead("Declare the membership", status("form-status")),
            note(
                "Every member at once, this node included, in one transaction: the first " +
                    "declaration clusters this node and costs it the authority to accept a " +
                    "second. A row with neither id nor fingerprint waits for a join token.",
            ),
            ...Array.from({ length: 5 }, (_, at) =>
                row(
                    "default",
                    field(
                        "Name",
                        text(`peer-${at}-name`, { placeholder: "warsaw" }),
                    ),
                    field(
                        "Peer address",
                        text(`peer-${at}-endpoint`, {
                            placeholder: "10.0.0.2:9000",
                        }),
                    ),
                    field(
                        "Client address",
                        text(`peer-${at}-clients`, {
                            placeholder: "10.0.0.2:9080",
                        }),
                    ),
                    field(
                        "Node id",
                        text(`peer-${at}-node`, { placeholder: "9f2c4e1a-…" }),
                    ),
                    field(
                        "or its fingerprint",
                        text(`peer-${at}-fingerprint`, { placeholder: "0f1e2d3c…" }),
                    ),
                    field(
                        "Replicates",
                        text(`peer-${at}-replicates`, { value: "STORE", size: 14 }),
                    ),
                    ...["serving", "writable", "coordinating"].map((bit) =>
                        field(
                            bit,
                            el("input", {
                                id: `peer-${at}-${bit}`,
                                type: "checkbox",
                            }),
                            { class: "tick" },
                        ),
                    ),
                ),
            ),
            says("form-says"),
            behindDisclosure(
                "The transaction this will send",
                el("div", { id: "form-statement", class: "answer small" }),
            ),
            row("default", button("form-cluster", "Declare it", "primary")),
        ),
        ...trustPanes(),
        pane(
            paneHead("What is here, and what is not"),
            note(
                "What the pane above shows is what this node itself knows: its own " +
                    "roles, the peers it has been told about, and the addresses it answers " +
                    "on. It is read from the node, not from anything standing beside it.",
            ),
            // 141 words of explanation, moved behind a disclosure rather than deleted.
            // The brief's rule is that prose paragraphs are not a UI element and that
            // explanation lives behind a disclosure, in the docs, or nowhere — and an
            // operator on their fiftieth visit is not reading this, while somebody on
            // their first may want it.
            behindDisclosure(
                "How membership works",
                note(
                    "Membership is a record and not a control plane: a node learns its peers " +
                        "through the same log it replicates data with, so there is nothing to stand " +
                        "up beside the database. A namespace declares how many copies the cluster " +
                        "keeps, and a peer's subscription decides which of them it receives. A " +
                        "leader holds a renewable lease granted by a majority and stops writing " +
                        "before that lease expires, measured on elapsed time rather than on the " +
                        "clock. A read may name the staleness it will accept, and a bound tighter " +
                        "than the cluster can know about itself is refused with the floor named. A " +
                        "write sent to a node that may not take it is forwarded to the one that may; " +
                        "one into a range another node leads, and a read only another node can " +
                        "answer, come back as a redirect carrying the client address, the node and " +
                        "the epoch it was decided under. A divergence is " +
                        "refused at the first divergent record and counted, never silently ranked.",
                ),
            ),
            note(
                el(
                    "strong",
                    {},
                    "A split table's shards and placements change while it serves.",
                ),
                " Each change is a statement, and the store's leader can make some itself.",
            ),
            behindDisclosure(
                "Which statements change them",
                note(
                    "ALTER TABLE … SPLIT AT and MERGE SHARD change the map in place, and " +
                        "SPLIT AUTOMATICALLY lets the store's leader do it by size and load. " +
                        "IDENTITY uuid SPREAD spreads new records over the shards, PARTITION BY " +
                        "gives each region its shard. ALTER REPLICA … LEADS moves a placement, " +
                        "LEADS NONE gives a range back to the store line, PREFERRED names the " +
                        "candidate that should lead it, and DEFINE FAILOVER … BALANCE LEADERSHIPS " +
                        "evens out who leads what.",
                ),
            ),
            note(
                el(
                    "strong",
                    {},
                    "Lag is per follower, and leadership is what the log recorded.",
                ),
            ),
            behindDisclosure(
                "What the two figures are, and are not",
                note(
                    "A follower's lag is how many records it is short of this node's tail, as " +
                        "this node last served it — the leader pushes each commit, so a level " +
                        "follower reads 0. A leadership is the row the winner wrote under its own " +
                        "epoch, so it says which node led a range as of that epoch; whether that " +
                        "node is alive now is its lease, which only it can report.",
                ),
            ),
        ),
    );
