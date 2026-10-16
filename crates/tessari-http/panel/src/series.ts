//! Series — which tables of a database are series, how each is ordered and how
//! long it answers, and the rollups kept of them.
//!
//! The markup only; the reading lives in `series-list.ts`. It sits on Data, with
//! the rest of what the store holds, rather than on a destination of its own.

import { type Node } from "./html.js";
import { answer, button, note, pane, paneHead, row, status } from "./ui.js";

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
    answer("series-list", "small"),
  );
