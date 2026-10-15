//! Topics — what each topic holds, who reads it, and the groups that settle it.
//!
//! The markup only. The reading lives in `topic-list.ts` and the forms that
//! change something in `topic-forms.ts`, so each file has one subject. Every
//! change is a dialog: a new topic from the list, and the chosen topic's groups
//! and removal from the topic's own pane.

import { el, type Node } from "./html.js";
import { to } from "./destinations.js";
import {
  answer, button, choose, dialog, field, note, number, pane, paneHead, panel, row, says,
  split, status, text,
} from "./ui.js";

const where = (): Node =>
  row(
    "default",
    field("Namespace", choose("topics-namespace", [])),
    field("Database", choose("topics-database", [])),
  );

const chosen = (): Node =>
  pane(
    paneHead(
      "Topic",
      el("span", { id: "topic-chosen", class: "faint" }, "none chosen"),
      row(
        "tight",
        button("open-new-group", "New group…", "quiet", { disabled: true }),
        button("open-group", "Move or remove a group…", "quiet", { disabled: true }),
        button("open-drop-topic", "Remove…", "quiet", { disabled: true, class: "quiet danger" }),
      ),
    ),
    status("topic-status"),
    answer("topic-facts", "small"),
    el("h3", {}, "Readers"),
    note("A reader keeps one position; its lag is how many messages it has still to read."),
    answer("topic-readers", "small"),
    el("h3", {}, "Groups"),
    note(
      "A group hands each message out and waits for it to be acknowledged. Its ",
      el("em", {}, "committed"),
      " position is the last one below which everything is settled, and its lag " +
        "counts from there.",
    ),
    answer("topic-groups", "small"),
    el("h3", {}, "Into tables"),
    note(
      "A topic consumer reads this topic as a group member and writes each message into a " +
        "table in the transaction that acknowledges it, so each message lands once. Declare " +
        "one on Run; the documentation's topics page has the statement.",
    ),
    answer("topic-ingested", "small"),
  );

const messages = (): Node =>
  pane(
    paneHead("Messages", status("browse-status")),
    note("Reading here moves nobody's position: it is a look, and every reader is left where it was."),
    row(
      "default",
      field("After position", number("browse-after", { value: 0, min: 0 })),
      field("How many", number("browse-count", { value: 20, min: 1, max: 100 })),
    ),
    row(
      "tight",
      button("browse-read", "Read", "default", { disabled: true }),
      button("browse-next", "Next page", "quiet", { disabled: true }),
    ),
    answer("browse-list"),
  );

const newTopic = (): Node =>
  dialog(
    "new-topic-dialog",
    "New topic",
    row(
      "default",
      field("Name", text("new-topic-name", { placeholder: "events" })),
      field("Keep for", text("new-topic-retain", { placeholder: "7d, or empty to keep all" })),
      field("Keep at most", text("new-topic-kept", { placeholder: "bytes, optional" })),
      field("Largest message", text("new-topic-bytes", { placeholder: "bytes, optional" })),
    ),
    says("new-topic-says"),
    row("spread", button("new-topic", "Create", "primary", { disabled: true }), status("new-topic-status")),
  );

const dropTopic = (): Node =>
  dialog(
    "drop-topic-dialog",
    "Remove the chosen topic",
    row(
      "default",
      field("Type its name to confirm", text("drop-topic-confirm")),
      field("Why", text("drop-topic-why", { placeholder: "replaced by events_v2" })),
    ),
    says("drop-topic-says"),
    row(
      "spread",
      button("drop-topic", "Remove", "default", { disabled: true, class: "danger" }),
      status("drop-topic-status"),
    ),
  );

const newGroup = (): Node =>
  dialog(
    "new-group-dialog",
    "New group on the chosen topic",
    row(
      "default",
      field("Group", text("new-group-name", { placeholder: "billing" })),
      field("Acknowledge within", text("new-group-deadline", { placeholder: "30s" })),
      field("In flight", text("new-group-width", { placeholder: "1" })),
    ),
    row(
      "default",
      field("Give up after", text("new-group-deliveries", { placeholder: "deliveries, optional" })),
      field("Dead letters to", text("new-group-dead", { placeholder: "a topic, optional" })),
    ),
    says("new-group-says"),
    row(
      "spread",
      button("new-group", "Create group", "primary", { disabled: true }),
      status("new-group-status"),
    ),
  );

const changeGroup = (): Node =>
  dialog(
    "group-dialog",
    "Move or remove a group",
    row(
      "default",
      field("Group", choose("group-which", [])),
      field(
        "Do",
        choose("group-action", [
          { value: "move", label: "hand out from a position", chosen: true },
          { value: "drop", label: "remove the group" },
        ]),
      ),
      field("After position", number("group-start", { value: 0, min: 0 })),
    ),
    row(
      "default",
      field("Type the group to confirm", text("group-confirm")),
      field("Why", text("group-why", { placeholder: "replay after the fix" })),
    ),
    says("group-says"),
    row("spread", button("group-apply", "Apply", "default", { disabled: true }), status("group-status")),
  );

export const topics = (): Node =>
  panel(
    to("topics"),
    false,
    paneHead(
      "Topics",
      row(
        "tight",
        status("topics-status"),
        button("topics-refresh", "Refresh", "quiet"),
        button("open-new-topic", "New topic…", "primary"),
      ),
    ),
    where(),
    answer("topics-list"),
    split(chosen(), messages()),
    newGroup(),
    changeGroup(),
    newTopic(),
    dropTopic(),
  );
