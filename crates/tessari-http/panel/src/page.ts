//! The console page, as a value.
//!
//! This file replaces 476 hand-typed lines of `index.html`. It is not shorter
//! by much and it was never meant to be: what it buys is that the next screen
//! is composed rather than typed, and that a tab which reaches no pane cannot
//! be written down.
//!
//! The prose is the panel's own, carried across word for word. It says what the
//! engine does and does not do, and rewriting it while moving it would put a
//! claim into the product that nobody decided.

import { document, el, type Node } from "./html.js";
import { access } from "./access.js";
import { cluster } from "./cluster.js";
import { DESTINATIONS, to } from "./destinations.js";
import {
  answer, behindDisclosure, button, field, note, number, pane, paneHead, panel,
  row, says, secret, split, status, tabs, text, warning,
} from "./ui.js";


const head = (): Node =>
  el(
    "head",
    {},
    el("meta", { charset: "utf-8" }),
    el("meta", { name: "viewport", content: "width=device-width, initial-scale=1" }),
    el("title", {}, "TessariDB console"),
    el("link", { rel: "icon", href: "/favicon.svg", type: "image/svg+xml" }),
    el("link", { rel: "stylesheet", href: "/console.css" }),
  );

const identity = (): Node =>
  el(
    "details",
    { class: "identity" },
    el("summary", {}, el("span", { id: "signed-in" }, "not signed in")),
    el(
      "div",
      { class: "sheet" },
      note(
        "Leave both empty against a store that has no users yet. The first ",
        el("code", {}, "DEFINE USER"),
        " closes it, and everything after that needs a name.",
      ),
      row(
        "default",
        field("User", el("input", { id: "user", type: "text", autocomplete: "username" })),
        field("Password", secret("password", "current-password")),
      ),
      note(
        "The password is checked once. What this page keeps afterwards is a token, " +
          "held for as long as the tab is open and no longer — so the password is " +
          "cleared from the field above, and closing the tab ends the session.",
      ),
      warning(
        "Credentials travel in the clear, exactly as they do for every other route " +
          "here: this store has no TLS and belongs on a network you protect.",
      ),
      row(
        "spread",
        row(
          "tight",
          button("sign-in", "Sign in", "primary"),
          button("sign-out", "Forget", "quiet"),
        ),
        status("identity-status"),
      ),
    ),
  );

const bar = (): Node =>
  el(
    "header",
    { class: "bar" },
    el(
      "div",
      { class: "who" },
      el("span", { class: "mark", "aria-hidden": "true" }),
      el("h1", {}, "TessariDB"),
      el("p", { id: "where", class: "faint" }, "served by this node"),
    ),
    // In the bar and not behind a destination. An operator arrives holding a
    // name, and an entry point you have to navigate to first is not one.
    el(
      "div",
      { class: "finding" },
      field(
        "Find",
        text("search", { placeholder: "an account, a table, app.main.orders:1  —  / or ⌘K" }),
      ),
      status("search-says"),
    ),
    identity(),
  );

const query = (): Node =>
  panel(
    to("run"),
    true,
    split(
      pane(
        paneHead("Script", el("span", { class: "faint" }, "⌘↵ or Ctrl↵ to run")),
        el(
          "textarea",
          { id: "script", rows: 8, spellcheck: "false", "aria-label": "Script" },
          "USE NAMESPACE prod; USE DATABASE library; SELECT * FROM users;",
        ),
        row("spread", button("run", "Run", "primary"), status("script-status")),
      ),
      pane(
        paneHead(
          "Answer",
          row(
            "tight",
            button(undefined, "Auto", "quiet", { class: "quiet chosen", "data-shape": "auto" }),
            button(undefined, "JSON", "quiet", { "data-shape": "json" }),
          ),
        ),
        answer("answer"),
      ),
    ),
    pane(
      paneHead("Follow a table", status("watch-status")),
      note(
        el("code", {}, "from"),
        " is a position in the log rather than a moment in time, so ",
        el("code", {}, "0"),
        " replays everything the log still holds. Leave the table empty to follow " +
          "every table you may read.",
      ),
      row(
        "default",
        field("Namespace", text("namespace", { value: "prod" })),
        field("Database", text("database", { value: "library" })),
        field("Table", text("table", { placeholder: "every table" })),
        field("From", number("from", { value: 0, min: 0 })),
      ),
      row(
        "default",
        button("follow", "Follow"),
        button("stop", "Stop", "quiet", { disabled: true }),
      ),
      el("ol", { id: "changes", class: "changes", reversed: true }),
    ),
  );


const node = (): Node =>
  panel(
    to("this-node"),
    false,
    paneHead(
      "This node",
      row("tight", status("node-status"), button("node-refresh", "Refresh", "quiet")),
    ),
    split(
      pane(paneHead("Identity and settings"), answer("node-facts", "small")),
      pane(
        paneHead("Health and readiness"),
        answer("node-health", "small"),
        note(
          "They are two answers, not one. A node says ",
          el("em", {}, "not ready"),
          " and goes on serving for five seconds while it drains, so whatever routes " +
            "traffic to it can stop doing so while it can still answer.",
        ),
      ),
    ),
    pane(paneHead("Metrics"), answer("node-metrics", "small")),
  );

/**
 * The statement log, and the one control that opens it.
 *
 * It sits in a tray at the foot of the page rather than in a pane, because it
 * belongs to the session and not to any one screen. Nothing routes through it
 * and it never opens itself — §5 of the brief is explicit that the record is
 * behind a control and never in the flow.
 */
/**
 * Every key the console answers to, in one place.
 *
 * Opened by `?` and by the control in the tray. A shortcut nobody can find is
 * not reachable, and the three the console already had were known only to
 * whoever wrote them.
 */
const keysSheet = (): Node =>
  el(
    "div",
    { id: "keys-sheet", class: "sheet keys-sheet", hidden: true },
    row("spread", el("h2", {}, "Keys"), button("keys-close", "Close", "quiet")),
    el("div", { id: "keys-list" }),
  );

/**
 * One thing, looked at — a sheet rather than a destination.
 *
 * The cap is four destinations and a detail view is not a place you go. It also
 * carries the sentence that keeps it honest: nothing here is a history, because
 * the store records no events for any of these objects.
 */
const detailSheet = (): Node =>
  el(
    "div",
    { id: "detail-sheet", class: "sheet detail-sheet", hidden: true },
    row(
      "spread",
      el(
        "h2",
        {},
        el("span", { id: "detail-kind", class: "faint" }, "thing"),
        " ",
        el("span", { id: "detail-name" }, ""),
      ),
      button("detail-close", "Close", "quiet"),
    ),
    el("div", { id: "detail-facts" }),
    note("There is no history of it: the store records no events for these."),
    behindDisclosure(
      "Why there is no timeline",
      note(
        "What is here is what the node answers about this right now. A timeline " +
          "drawn from anything else would be this panel inventing one.",
      ),
    ),
  );

const tray = (): Node =>
  el(
    "footer",
    { class: "tray" },
    // Built with `el` rather than `ui.button`, which takes a label and not
    // markup: the count is a live element the log writes into, so it has to be
    // a child rather than part of a string.
    el(
      "button",
      {
        id: "log-open",
        type: "button",
        class: "quiet",
        "aria-controls": "log-sheet",
        "aria-expanded": "false",
      },
      "Statements ",
      el("span", { id: "log-count", class: "count" }, "0"),
    ),
    el("span", { class: "faint" }, "everything this panel sent for you"),
    // The keys, reachable by mouse as well as by the key that opens them —
    // a shortcut list you can only open with a shortcut is a joke played on
    // exactly the person who needs it.
    el(
      "button",
      { id: "keys-open", type: "button", class: "quiet", "aria-controls": "keys-sheet" },
      "Keys ?",
    ),
  );

const logSheet = (): Node =>
  el(
    "div",
    { id: "log-sheet", class: "sheet log", hidden: true, role: "region", "aria-label": "Statements" },
    paneHead("Statements this session", button("log-close", "Close", "quiet")),
    note(
      "The panel's own record, newest first — not the store's, and it lasts as " +
        "long as this tab.",
    ),
    behindDisclosure(
      "How this differs from the store's audit",
      note(
        el("code", {}, "INFO FOR AUDIT"),
        " answers a different question, for a different reader, and keeps its " +
          "answer. This holds no credential and makes no claim to survive the tab.",
      ),
    ),
    el("div", { id: "log-list" }),
  );

export const index = (): string =>
  document(
    el(
      "html",
      { lang: "en" },
      head(),
      el(
        "body",
        {},
        bar(),
        tabs(DESTINATIONS),
        // In the tab strip's own order, so the reading order of the page and the
        // order of the controls above it are one thing rather than two.
        el("main", {}, query(), cluster(), access(), node()),
        tray(),
        logSheet(),
        keysSheet(),
        detailSheet(),
        el("script", { src: "/console.js" }),
      ),
    ),
  );
