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

import { valueOf } from "./api.js";
import { at, clear, made, say, trimmed } from "./dom.js";
import { told } from "./session.js";
import { quoted } from "./user-forms.js";

/** One row of the intended membership, as the form holds it. */
interface Intended {
  readonly name: string;
  readonly endpoint: string;
  readonly node: string;
  readonly roles: readonly string[];
}

/** How many rows the form offers. A membership larger than this is a script. */
const ROWS = 5;

const rowFields = (at_: number): readonly string[] => [
  `peer-${at_}-name`,
  `peer-${at_}-endpoint`,
  `peer-${at_}-node`,
];

/** The rows the operator actually filled in, in the order they appear. */
function intended(): readonly Intended[] {
  const found: Intended[] = [];
  for (let index = 0; index < ROWS; index += 1) {
    const name = trimmed(`peer-${index}-name`);
    const endpoint = trimmed(`peer-${index}-endpoint`);
    const node = trimmed(`peer-${index}-node`);
    if (name === "" && endpoint === "" && node === "") {
      continue;
    }
    const roles: string[] = [];
    for (const bit of ["serving", "writable", "coordinating"]) {
      if ((at(`peer-${index}-${bit}`) as HTMLInputElement).checked) {
        roles.push(bit);
      }
    }
    found.push({ name, endpoint, node, roles });
  }
  return found;
}

/** Which row is incomplete, named so the operator does not hunt for it. */
function incomplete(rows: readonly Intended[]): string | null {
  for (const [index, row] of rows.entries()) {
    const missing =
      row.name === ""
        ? "a name"
        : row.endpoint === ""
          ? "an address"
          : row.node === ""
            ? "a node id"
            : row.roles.length === 0
              ? "at least one role"
              : null;
    if (missing !== null) {
      return `row ${index + 1} needs ${missing}`;
    }
  }
  return null;
}

/**
 * The one transaction this form sends.
 *
 * Exported so the preview and the button read the same value rather than two
 * that agree today: the whole point of this form is that what is confirmed is
 * what runs.
 */
export function formation(rows: readonly Intended[]): string {
  const declarations = rows.map(
    (row) =>
      `DEFINE REPLICA ${row.name} AT ${quoted(row.endpoint)} ` +
      `NODE ${quoted(row.node)} ROLES ${row.roles.join(", ")};`,
  );
  return ["BEGIN;", ...declarations, "COMMIT;"].join("\n");
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
  say(
    "form-says",
    `Declares ${rows.length === 1 ? "one peer" : `${rows.length} peers`} — ${named} — ` +
      "in a single transaction. All of them or none.",
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

export function wire(): void {
  for (let index = 0; index < ROWS; index += 1) {
    for (const field of [
      ...rowFields(index),
      `peer-${index}-serving`,
      `peer-${index}-writable`,
      `peer-${index}-coordinating`,
    ]) {
      at(field).addEventListener("input", () => {
        preview();
        showStatement();
      });
      at(field).addEventListener("change", () => {
        preview();
        showStatement();
      });
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
      say("form-status", answered !== null && answered.kind === "done" ? "declared" : "");
    } catch (failure) {
      // The node's own words. A refusal here is usually the fence explaining
      // itself, and paraphrasing it would hide which refusal it was.
      say("form-status", told(failure), true);
    }
  });

  preview();
  showStatement();
}
