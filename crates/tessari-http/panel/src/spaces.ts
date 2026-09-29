//! Spaces — what a space holds: its keys by prefix, and one key's value and how
//! long it has left (ADR-0090, G046).
//!
//! The markup only; the reading lives in `spaces-list.ts`. It sits on Run beside
//! Series, for the reason Series gives: a space is data you query there, and the
//! destinations are capped at five.

import { type Node } from "./html.js";
import { answer, button, choose, field, note, pane, paneHead, row, status, text } from "./ui.js";

export const spaces = (): Node =>
  pane(
    paneHead(
      "Spaces",
      row("tight", status("kv-status"), button("kv-refresh", "Refresh", "quiet")),
    ),
    note(
      "A space keeps one value per key. A plain write clears the key's expiry; a " +
        "lock's value names its holder until its lease runs out.",
    ),
    row(
      "default",
      field("Namespace", choose("kv-namespace", [])),
      field("Database", choose("kv-database", [])),
      field("Space", choose("kv-space", [])),
    ),
    row(
      "default",
      field("Prefix", text("kv-prefix", { placeholder: "every key" })),
      button("kv-list-them", "List", "quiet"),
    ),
    answer("kv-list", "small"),
    answer("kv-value", "small"),
  );
