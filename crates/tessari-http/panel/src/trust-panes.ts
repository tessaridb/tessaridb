//! The cluster tab's trust panes: what this node presents, what the cluster
//! refuses, who may join, and the periods every node agrees on (ADR-0108).
//!
//! Markup only; `trust.ts` reads the node and composes the statements.

import { el, type Node } from "./html.js";
import { button, choose, field, note, pane, paneHead, row, says, status, text } from "./ui.js";

/** How long a join token may bind, offered rather than typed: a token is a secret with a clock. */
const LIVES = [
  { value: "10m", label: "10 minutes", chosen: true },
  { value: "1h", label: "1 hour" },
  { value: "24h", label: "24 hours" },
];

const certificates = (): Node =>
  pane(
    paneHead("Certificates and trust", status("trust-status")),
    note(
      "What this node presents now; a renewal shows from its next connection on. A " +
        "newcomer's peer fingerprint pins its row, read off the newcomer like its node id.",
    ),
    el("div", { id: "trust-mine" }),
    el("h3", {}, "Refused certificates"),
    el("div", { id: "trust-revoked" }),
    el("h3", {}, "Removed nodes"),
    el("div", { id: "trust-removed" }),
  );

const revoke = (): Node =>
  pane(
    paneHead("Revoke a certificate", status("revoke-status")),
    note(
      "Every node refuses it in both directions of the peer link within seconds, and a " +
        "held stream presenting it ends. A revocation is permanent: its holder is given a " +
        "new certificate.",
    ),
    row(
      "default",
      field("Fingerprint (SHA-256)", text("revoke-fingerprint", { placeholder: "0f1e2d3c…", size: 66 })),
    ),
    row(
      "default",
      field("Type its first eight digits again", text("revoke-confirm", { size: 10 })),
      field("Why", text("revoke-why", { placeholder: "laptop lost" })),
    ),
    says("revoke-says"),
    row("default", button("revoke-apply", "Revoke", "default", { disabled: true })),
  );

const join = (): Node =>
  pane(
    paneHead("Approve a joining node", status("join-status")),
    note(
      "For a row declared without a node id or a fingerprint. The token is shown once and " +
        "the node keeps only its digest; start the newcomer with --join-token beside its --seed.",
    ),
    row(
      "default",
      field("Row", choose("join-row", [])),
      field("Binds for", choose("join-life", LIVES)),
    ),
    says("join-says"),
    row("default", button("join-apply", "Issue a token", "default", { disabled: true })),
    el("pre", { id: "join-token", class: "answer small", hidden: true }),
  );

/** One duration field of the failover form; the placeholder is the built-in period. */
const period = (clause: string, hint: string): Node =>
  field(clause, text(`failover-${clause.toLowerCase()}`, { placeholder: hint, size: 6 }));

const failover = (): Node =>
  pane(
    paneHead("Failover policy", status("failover-status")),
    el("p", { id: "failover-held", class: "note" }),
    row(
      "default",
      period("AWARENESS", "1s"),
      period("COLLECTION", "1s"),
      period("ROUND", "200ms"),
      period("CAMPAIGN", "100ms"),
      period("LEASE", "800ms"),
      // The statement replaces the whole set, so leaving this unticked turns
      // the leadership balancer off (ADR-0113 D3).
      field(
        "balance leaderships",
        el("input", { id: "failover-balance", type: "checkbox" }),
        { class: "tick" },
      ),
    ),
    says("failover-says"),
    row("default", button("failover-apply", "Set the policy", "default", { disabled: true })),
  );

export const trustPanes = (): readonly Node[] => [certificates(), revoke(), join(), failover()];
