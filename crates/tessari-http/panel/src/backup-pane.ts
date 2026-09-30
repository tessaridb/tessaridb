//! Backup — the store written to a file in the node's backup folder.
//!
//! The markup only; what the button sends is in `backup.ts`.

import { el, type Node } from "./html.js";
import { to } from "./destinations.js";
import { answer, button, choose, field, note, pane, paneHead, panel, row, says, status, text } from "./ui.js";

export const backup = (): Node =>
  panel(
    to("backup"),
    false,
    paneHead("Backup", status("backup-status")),
    pane(
      note(
        "The node writes the file into its backup folder (",
        el("code", {}, "--backup-dir"),
        ", or ",
        el("code", {}, "TESSARIDB_BACKUP_DIR"),
        " in the image). The name may include subfolders and never replaces an existing " +
          "file. Store-wide owners only.",
      ),
      row(
        "default",
        field(
          "What to write",
          choose("backup-form", [
            { value: "state", label: "the current state — a snapshot", chosen: true },
            { value: "log", label: "the log — every commit, in order" },
            { value: "script", label: "the current state as TessariQL" },
          ]),
        ),
        field("File name", text("backup-name", { spellcheck: false, autocomplete: "off" })),
      ),
      row(
        "default",
        field(
          "What to include",
          choose("backup-part", [
            { value: "store", label: "the whole store", chosen: true },
            { value: "places", label: "chosen namespaces and databases" },
          ]),
        ),
        field(
          "Namespaces and databases",
          text("backup-places", { placeholder: "crm, prod.orders", spellcheck: false, autocomplete: "off" }),
        ),
      ),
      says("backup-says"),
      row("default", button("backup-run", "Back up", "primary", { disabled: true })),
      answer("backup-answer", "small"),
    ),
    pane(
      paneHead("Restore a script", status("restore-status")),
      note(
        "Runs a script from the same folder beside what the store holds: it creates the databases " +
          "it carries and is refused, with nothing written, when one of them already exists.",
      ),
      row("default", field("File name", text("restore-name", { spellcheck: false, autocomplete: "off" }))),
      says("restore-says"),
      row("default", button("restore-run", "Restore", "default", { disabled: true })),
      answer("restore-answer", "small"),
    ),
  );
