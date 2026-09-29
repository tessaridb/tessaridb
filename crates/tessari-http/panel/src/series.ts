//! Series — which tables of a database are series, how each is ordered and how
//! long it answers, and the rollups kept of them.
//!
//! The markup only; the reading lives in `series-list.ts`. It sits on Run rather
//! than on a destination of its own: a series is data you query there, and the
//! destinations are capped at five (see `destinations.ts`).

import { type Node } from "./html.js";
import { answer, button, choose, field, note, pane, paneHead, row, status } from "./ui.js";

export const series = (): Node =>
  pane(
    paneHead(
      "Series",
      row("tight", status("series-status"), button("series-refresh", "Refresh", "quiet")),
    ),
    note(
      "A series ordered by its time field keeps each event where it happened; one " +
        "ordered by arrival keeps it where it was written. A rollup is kept by the " +
        "writes to its series.",
    ),
    row(
      "default",
      field("Namespace", choose("series-namespace", [])),
      field("Database", choose("series-database", [])),
    ),
    answer("series-list", "small"),
  );
