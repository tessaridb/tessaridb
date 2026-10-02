//! Declaring the cluster's membership, in one transaction.
//!
//! # The cliff this form exists to not build
//!
//! Declaring peers one statement at a time STRANDS the operator, and it was
//! reproduced twice against a running node:
//!
//! ```text
//! DEFINE REPLICA warsaw …;  → ok
//! DEFINE REPLICA lisbon …;  → error: this node is in a cluster and holds no
//!                             leadership: it does not accept writes until a
//!                             majority grants it one
//! ```
//!
//! The first declaration makes the node clustered, which costs it `writable`,
//! which is what the second declaration needs. Wrapping both in
//! `BEGIN; … COMMIT;` succeeds.
//!
//! So a per-row Add button would build that cliff into the interface, and the
//! operator would meet it halfway through a membership with no way forward and
//! no way back. The whole intended membership is one form and one transaction.
//!
//! # Not a wizard either
//!
//! There are no ordered stages here — a membership is a set, declared at once.
//! A wizard would impose an order the domain does not have and would make the
//! last step the one that fails.
//!
//! # What a row must say, and what the form adds
//!
//! A row with no `REPLICATES` is subscribed to nothing — every node up, every
//! greeting landing, one copy that never changes — so the subscription is a
//! field with `STORE` already in it rather than a clause left to memory. The
//! identity is a node id or a pinned certificate fingerprint (ADR-0108 D9); a
//! row with neither binds nobody until a join token is issued for it below.
//!
//! This node's own row is offered first, filled from the node. After the
//! transaction the form sets this node's roles to what its row declares, with
//! `DEFINE NODE ROLES` — local and never fenced — because a node left at the
//! default `serving, writable` is clustered and a candidate for nothing: it
//! stops accepting writes and never starts again.



import { valueOf } from "./api.js";
import { at, clear, made, say, setValue, trimmed } from "./dom.js";
import { told } from "./session.js";
import { hereAgain } from "./tabs.js";
import { aName } from "./topic-names.js";
import { aFingerprint } from "./trust.js";
import { quoted } from "./user-forms.js";

/** One row of the intended membership, as the form holds it. */
interface Intended {
  readonly name: string;
  readonly endpoint: string;
  readonly clients: string;
  readonly node: string;
  readonly fingerprint: string;
  readonly replicates: string;
  readonly roles: readonly string[];
}

/** How many rows the form offers. A membership larger than this is a script. */
const ROWS = 5;
const BITS = ["serving", "writable", "coordinating"] as const;
/** The fields that say a row is there at all; `replicates` starts filled. */
const IDENTIFYING = ["name", "endpoint", "clients", "node", "fingerprint"] as const;
const TEXTS = [...IDENTIFYING, "replicates"] as const;

/**
 * What `REPLICATES` may say here: the store, a namespace or a database. A reach
 * is grammar, so it is checked narrower than the node's lexer and never quoted.
 */
const REACH = /^(STORE|NAMESPACE [A-Za-z_][A-Za-z0-9_]*|DATABASE [A-Za-z_][A-Za-z0-9_]*\.[A-Za-z_][A-Za-z0-9_]*)$/i;

/** This node, as the last `INFO FOR NODE` reported it. */
let thisNode: { readonly id: string; readonly endpoint: string } | null = null;

/** The rows the operator actually filled in, in the order they appear. */
function intended(): readonly Intended[] {
  const found: Intended[] = [];
  for (let index = 0; index < ROWS; index += 1) {
    const read = (part: (typeof TEXTS)[number]): string => trimmed(`peer-${index}-${part}`);
    if (IDENTIFYING.every((part) => read(part) === "")) {
      continue;
    }
    const roles = BITS.filter((bit) => (at(`peer-${index}-${bit}`) as HTMLInputElement).checked);
    found.push({
      name: read("name"),
      endpoint: read("endpoint"),
      clients: read("clients"),
      node: read("node"),
      fingerprint: read("fingerprint"),
      replicates: read("replicates").replace(/\s+/g, " "),
      roles,
    });
  }
  return found;
}

/** Which row is incomplete, named so the operator does not hunt for it. */
function incomplete(rows: readonly Intended[]): string | null {
  for (const [index, row] of rows.entries()) {
    const missing =
      aName(row.name) === null
        ? "a name: a letter or _, then letters, digits or _"
        : row.endpoint === ""
          ? "a peer address"
          : row.node !== "" && row.fingerprint !== ""
            ? "a node id or a fingerprint, not both"
            : row.fingerprint !== "" && aFingerprint(row.fingerprint) === null
              ? "a fingerprint of 64 hexadecimal digits"
              : !REACH.test(row.replicates)
                ? "what it replicates: STORE, NAMESPACE n or DATABASE n.d"
                : row.roles.length === 0
                  ? "at least one role"
                  : null;
    if (missing !== null) {
      return `row ${index + 1} needs ${missing}`;
    }
  }
  return null;
}

/** The row that declares this node, when one does. */
const own = (rows: readonly Intended[]): Intended | undefined =>
  thisNode === null ? undefined : rows.find((row) => row.node === thisNode?.id);

/**
 * The script this form sends: one transaction, then this node's own roles.
 *
 * Exported so the preview and the button read the same value rather than two
 * that agree today: the whole point of this form is that what is confirmed is
 * what runs.
 */
export function formation(rows: readonly Intended[]): string {
  const declarations = rows.map((row) => {
    const pinned = aFingerprint(row.fingerprint);
    return (
      `DEFINE REPLICA ${row.name} AT ${quoted(row.endpoint)}` +
      (row.clients === "" ? "" : ` CLIENTS AT ${quoted(row.clients)}`) +
      (row.node === "" ? "" : ` NODE ${quoted(row.node)}`) +
      ` ROLES ${row.roles.join(", ")} REPLICATES ${row.replicates}` +
      (pinned === null ? "" : ` FINGERPRINT '${pinned}'`) +
      ";"
    );
  });
  const mine = own(rows);
  return [
    "BEGIN;",
    ...declarations,
    "COMMIT;",
    ...(mine === undefined ? [] : [`DEFINE NODE ROLES ${mine.roles.join(", ")};`]),
  ].join("\n");
}

/** What the button will do, in words, and what is still missing. */
function preview(): void {
  const rows = intended();
  const missing = incomplete(rows);
  if (rows.length === 0) {
    say("form-says", "Nothing declared yet.");
    return;
  }
  if (missing !== null) {
    say("form-says", missing, true);
    return;
  }
  const named = rows.map((row) => row.name).join(", ");
  const waiting = rows.filter((row) => row.node === "" && row.fingerprint === "").map((row) => row.name);
  const mine = own(rows);
  say(
    "form-says",
    `Declares ${rows.length === 1 ? "one member" : `${rows.length} members`} — ${named} — ` +
      "in a single transaction. All of them or none. " +
      (mine === undefined
        ? "This node keeps the roles it has, as none of these rows names it — and once " +
          "clustered, a node writes only while it holds coordinating. "
        : mine.roles.includes("coordinating")
          ? `Then sets this node's roles to ${mine.roles.join(", ")}. `
          : "This node's row leaves out coordinating: once clustered, it stops accepting writes for good. ") +
      (waiting.length === 0 ? "" : `${waiting.join(", ")} will wait for a join token.`),
  );
}

/** The statement, kept reachable under the disclosure. */
function showStatement(): void {
  const rows = intended();
  clear("form-statement");
  const block = made("pre");
  block.textContent = rows.length === 0 || incomplete(rows) !== null ? "" : formation(rows);
  at("form-statement").appendChild(block);
}

function changed(): void {
  preview();
  showStatement();
}

/**
 * Offer this node as the first row, once, while the form is untouched — so the
 * membership an operator declares includes the node they are declaring it on.
 */
export function know(id: unknown, endpoints: unknown, peers: number): void {
  if (typeof id !== "string") {
    return;
  }
  const endpoint = Array.isArray(endpoints) && typeof endpoints[0] === "string" ? endpoints[0] : "";
  thisNode = { id, endpoint };
  // Once a membership exists this node's row is in it, and offering it again
  // would compose a second declaration of a name already in use.
  if (peers > 0 || IDENTIFYING.some((part) => trimmed(`peer-0-${part}`) !== "")) {
    changed();
    return;
  }
  setValue("peer-0-name", "this_node");
  setValue("peer-0-endpoint", endpoint);
  setValue("peer-0-node", id);
  for (const bit of BITS) {
    (at(`peer-0-${bit}`) as HTMLInputElement).checked = true;
  }
  changed();
}

export function wire(): void {
  for (let index = 0; index < ROWS; index += 1) {
    for (const part of [...TEXTS, ...BITS]) {
      at(`peer-${index}-${part}`).addEventListener("input", changed);
      at(`peer-${index}-${part}`).addEventListener("change", changed);
    }
  }

  at("form-cluster").addEventListener("click", async () => {
    const rows = intended();
    if (rows.length === 0) {
      say("form-status", "nothing to declare", true);
      return;
    }
    const missing = incomplete(rows);
    if (missing !== null) {
      say("form-status", missing, true);
      return;
    }
    say("form-status", "running…");
    try {
      const answered = await valueOf(formation(rows), "Cluster · form");
      const done = answered !== null && answered.kind === "done";
      say("form-status", done ? "declared" : "");
      if (done) {
        // Emptied, so the button cannot declare the same rows twice.
        for (let index = 0; index < ROWS; index += 1) {
          for (const part of IDENTIFYING) {
            setValue(`peer-${index}-${part}`, "");
          }
          setValue(`peer-${index}-replicates`, "STORE");
          for (const bit of BITS) {
            (at(`peer-${index}-${bit}`) as HTMLInputElement).checked = false;
          }
        }
        changed();
        // The map is two inches above this button and was drawn before the
        // membership that now exists. W319 fixed exactly this for the drawer
        // and did not reach the form; W320 declared a peer here, read
        // `declared`, and watched the map go on showing a single node — with
        // the node itself, asked over HTTP, already holding the peer.
        hereAgain();
      }
    } catch (failure) {
      // The node's own words. A refusal here is usually the fence explaining
      // itself, and paraphrasing it would hide which refusal it was.
      say("form-status", told(failure), true);
    }
  });

  changed();
}
