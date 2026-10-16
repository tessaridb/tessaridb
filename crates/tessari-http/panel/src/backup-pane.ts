//! Backup — the store written to a file in the node's backup folder.
//!
//! The markup only; what the button sends is in `backup.ts`.

import { el, type Node } from "./html.js";
import { to } from "./destinations.js";
import { answer, button, choose, dialog, field, note, pane, paneHead, panel, row, says, status, text } from "./ui.js";

export const backup = (): Node =>
  panel(
    to("backup"),
    false,
    paneHead(
      "Backup",
      row(
        "tight",
        button("open-backup", "Back up…", "primary"),
        button("open-restore", "Restore a script…", "quiet"),
      ),
    ),
    pane(
      note(
        "Each runs in its own dialog, and the node answers there: the file it wrote, or what it ran.",
      ),
    ),
    dialog(
      "backup-dialog",
      "Back up",
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
      row("spread", button("backup-run", "Back up", "primary", { disabled: true }), status("backup-status")),
      answer("backup-answer", "small"),
    ),
    dialog(
      "restore-dialog",
      "Restore a script",
      note(
        "Runs a script from the same folder beside what the store holds: it creates the databases " +
          "it carries and is refused, with nothing written, when one of them already exists.",
      ),
      row("default", field("File name", text("restore-name", { spellcheck: false, autocomplete: "off" }))),
      says("restore-says"),
      row("spread", button("restore-run", "Restore", "default", { disabled: true }), status("restore-status")),
      answer("restore-answer", "small"),
    ),
  );
