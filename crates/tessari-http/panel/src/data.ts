//! Data — what the store holds: its namespaces, their databases, and the tables in each.
//!
//! The markup only; the reading is `data-list.ts`. It changes nothing: a table is
//! looked at in a dialog, or opened on Run with the statement that reads it.

import { el, type Node } from "./html.js";
import { to } from "./destinations.js";
import { series } from "./series.js";
import { spaces } from "./spaces.js";
import { answer, button, choose, dialog, field, pane, panel, paneHead, row, status } from "./ui.js";

export const data = (): Node =>
  panel(
    to("data"),
    false,
    row(
      "default",
      field("Namespace", choose("data-namespace", [])),
      field("Database", choose("data-database", [])),
    ),
    pane(
      paneHead(
        "Tables",
        row("tight", status("data-status"), button("data-refresh", "Refresh", "quiet")),
      ),
      answer("data-list"),
    ),
    dialog(
      "table-dialog",
      "Table",
      status("table-status"),
      answer("table-facts", "small"),
      el("pre", { id: "table-definition", class: "answer small" }),
      row("default", button("table-browse", "Open on Run", "primary")),
    ),
    series(),
    spaces(),
  );
