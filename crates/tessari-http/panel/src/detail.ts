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
//! # A record has a timeline; the others still do not
//!
//! T5.2 asked for a timeline on each of these, and W318 drew none because the
//! measurement said there was no event source. That measurement was of three
//! READ surfaces — `INFO FOR VERSIONS` (a conflict report: one version after
//! three writes), `INFO FOR NAMESPACE` (a list of databases) and `INFO FOR
//! AUDIT` (the vault's own trail) — and their silence was read as the store
//! recording nothing. It records everything: every commit is a log record
//! carrying what it changed (Q-739). What was missing was a way to ask, and
//! `INFO FOR HISTORY OF` is now it.
//!
//! So a RECORD is drawn with its history. A namespace, a database and a table
//! are not, and that is not an omission left for later: a catalog row is
//! deliberately filtered out of the log projection, so there is genuinely
//! nothing to draw for them, and the sheet says which case it is in rather than
//! showing an empty list that reads like a quiet record.
//!
//! # Three states, three renderings
//!
//! A timeline nobody could fetch, a timeline that is empty, and a timeline cut
//! short at the log's walk budget are three different facts. Rendering any two
//! of them the same way is the failure this console has refused by name three
//! times: an absence that looks like a measurement.

import { at, clear, made } from "./dom.js";
import { facts } from "./draw.js";
import type { Result } from "./api.js";

/** One entry of a record's history, as the node answers it. */
interface Event {
  readonly at?: unknown;
  readonly change?: unknown;
  readonly value?: unknown;
}

/**
 * Draw the history the node answered, or say which kind of nothing this is.
 *
 * `history` is `null` when the question could not be asked or was refused —
 * which is not the same as a record nothing has happened to, and the two say
 * different things here.
 */
function timeline(kind: string, history: Result | null): void {
  clear("detail-history");
  const line = made("p", "note");
  if (kind !== "record") {
    line.textContent =
      "Only a record has a history: a catalog row is kept out of the log\u2019s " +
      "change projection, so there is nothing recorded to draw for this.";
    at("detail-history").appendChild(line);
    return;
  }
  const answer = history?.value as Record<string, unknown> | undefined;
  const events = Array.isArray(answer?.events) ? (answer.events as Event[]) : null;
  if (events === null) {
    line.textContent =
      "The history could not be read \u2014 the node refused it, or this build " +
      "does not answer INFO FOR HISTORY. That is not the same as nothing having " +
      "happened.";
    at("detail-history").appendChild(line);
    return;
  }
  if (events.length === 0) {
    line.textContent = "Nothing is recorded against this record in the log read.";
    at("detail-history").appendChild(line);
    return;
  }
  const list = made("ol", "timeline");
  for (const event of events) {
    const entry = made("li");
    const what = event.change === "removed" ? "removed" : "written";
    const when = typeof event.at === "string" ? event.at : "?";
    entry.textContent = `${what} at ${when}`;
    list.appendChild(entry);
  }
  at("detail-history").appendChild(list);
  // Truncation is REPORTED and never implied by a short list. A console showing
  // five entries and letting a reader take them for all of them is exactly the
  // dashboard-ahead-of-the-engine failure this panel refuses elsewhere.
  if (answer?.complete === false) {
    const cut = made("p", "note");
    cut.textContent =
      "Older entries may exist: the read stopped at its record budget before " +
      "reaching the start of the log.";
    at("detail-history").appendChild(cut);
  }
}

/** Open the sheet on one thing. */
export function show(
  kind: string,
  name: string,
  answered: Result | null,
  history: Result | null = null,
): void {
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
  timeline(kind, history);
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
