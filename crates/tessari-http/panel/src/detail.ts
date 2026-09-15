//! One thing, looked at.
//!
//! A sheet over whatever you were doing, not a destination. The cap is four
//! destinations and a detail view is not a place you go — it is a thing you
//! open, look at, and close, and it has to leave you where you were.
//!
//! It draws whatever the node answered, as facts, under a heading that names
//! what kind of thing it is. The same sheet serves a namespace, a database, a
//! table and a record, because the difference between them is the question that
//! was asked and not the shape of the answer.
//!
//! # No timeline, and the screen says why
//!
//! T5.2 asked for a timeline on each of these. Measured on a running node, there
//! is no event source for any of them: `INFO FOR VERSIONS` answers ONE version
//! after three writes — it reports whether a record is CONTESTED, never what
//! happened to it — `INFO FOR NAMESPACE` answers a list of databases, and
//! `INFO FOR AUDIT` is the vault's audit and holds recorded vault reads alone.
//!
//! So nothing here is drawn as a history. A timeline assembled on the client
//! would be the console inventing an event nobody recorded, which is the same
//! failure as a replication-lag figure with no follower loop behind it — and
//! that one this console already refuses by name.

import { at, clear, made } from "./dom.js";
import { facts } from "./draw.js";
import type { Result } from "./api.js";

/** Open the sheet on one thing. */
export function show(kind: string, name: string, answered: Result | null): void {
  at("detail-kind").textContent = kind;
  at("detail-name").textContent = name;
  clear("detail-facts");
  // Two answer shapes reach here and both are ordinary. An `INFO FOR …` answers
  // a `value` object; a `SELECT` answers `records`, each with its own id. The
  // sheet draws either, because the difference is the question that was asked
  // and not the kind of thing being looked at — which is the whole reason one
  // sheet serves a namespace, a table and a record.
  if (Array.isArray(answered?.records)) {
    for (const row of answered.records) {
      const id = made("p", "faint");
      id.textContent = row.id;
      at("detail-facts").appendChild(id);
      if (typeof row.value === "object" && row.value !== null) {
        const into = made("div");
        into.id = `detail-row-${row.id.replace(/[^a-zA-Z0-9]/g, "-")}`;
        at("detail-facts").appendChild(into);
        facts(into.id, row.value as Record<string, unknown>);
      }
    }
    if (answered.records.length === 0) {
      const nothing = made("p", "note");
      nothing.textContent = "The node answered, and there is no such record.";
      at("detail-facts").appendChild(nothing);
    }
  } else if (typeof answered?.value === "object" && answered.value !== null) {
    facts("detail-facts", answered.value as Record<string, unknown>);
  } else {
    const nothing = made("p", "note");
    nothing.textContent = "The node answered, and the answer carries no fields.";
    at("detail-facts").appendChild(nothing);
  }
  at("detail-sheet").hidden = false;
  at("detail-close").focus();
}

function closeIt(): void {
  at("detail-sheet").hidden = true;
}

/** Shut it without being asked — before a new question is put to the node. */
export const hide = closeIt;

export function wire(): void {
  at("detail-close").addEventListener("click", closeIt);
  document.addEventListener("keydown", (pressed) => {
    if (pressed.key === "Escape" && !at("detail-sheet").hidden) {
      closeIt();
    }
  });
}
