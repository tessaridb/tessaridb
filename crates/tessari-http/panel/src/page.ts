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
import {
  answer, button, choose, field, note, number, pane, paneHead, panel, preview,
  ROLES, row, secret, split, status, tabs, text, warning, type Destination,
} from "./ui.js";

const DESTINATIONS: readonly Destination[] = [
  { name: "query", label: "Query" },
  { name: "users", label: "Users" },
  { name: "node", label: "Node" },
  { name: "cluster", label: "Cluster" },
];

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
    identity(),
  );

const query = (): Node =>
  panel(
    DESTINATIONS[0]!,
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

const whoThereIs = (): Node =>
  pane(
    paneHead("Who there is", row("tight", status("user-status"), button("list", "Refresh", "quiet"))),
    note(
      "What you are shown is the tenancy you administer. An owner of a space sees " +
        "that space; an ",
      el("strong", {}, "admin"),
      " — an owner with no space at all — sees the whole node. It refuses rather " +
        "than narrowing, so a short list is a short tenancy and never a filtered one.",
    ),
    answer("user-list", "small"),
    row(
      "default",
      field("Detail for", text("lookup-name", { placeholder: "ada" })),
      button("lookup", "Show"),
    ),
    answer("user-answer", "small"),
  );

const defineOne = (): Node =>
  pane(
    paneHead("Define one", status("define-status")),
    note(
      "A password reaches the node and goes no further: what is stored is a hash, " +
        "so no plaintext reaches the log or any replica.",
    ),
    row(
      "default",
      field("Name", text("new-name", { placeholder: "ada" })),
      field(
        "Reach ",
        choose("new-reach", [
          { value: "space", label: "one space" },
          { value: "node", label: "the whole node — an admin" },
        ]),
      ),
      field("Space", text("new-scope", { placeholder: "prod.library" }), { id: "scope-field" }),
      field("Role", choose("new-role", ROLES.map((role) =>
        role.value === "editor" ? { ...role, chosen: true } : role,
      ))),
      field("Which", text("new-role-other", { placeholder: "a role this build knows" }), {
        id: "role-other-field",
        hidden: true,
      }),
      field("Password", secret("new-password", "new-password")),
    ),
    el("p", { id: "role-says", class: "note" }),
    preview("define-preview"),
    row("default", button("define", "Run it", "primary")),
  );

const changeOne = (): Node =>
  pane(
    paneHead("Change one", status("change-status")),
    note(
      "One field per change, and the other is left exactly as it was: a password " +
        "rotation does not touch a role, and a role correction does not invalidate " +
        "a password. A space cannot be changed here at all — widening somebody's " +
        "reach is the one change an owner of a part could use to reach the whole, " +
        "so it is not offered.",
    ),
    row(
      "default",
      field("Who", text("change-name", { placeholder: "ada" })),
      field(
        "What ",
        choose("change-what", [
          { value: "password", label: "password" },
          { value: "role", label: "role" },
        ]),
      ),
      field("New password", secret("change-password", "new-password"), {
        id: "change-password-field",
      }),
      field("New role", choose("change-role", ROLES), {
        id: "change-role-field",
        hidden: true,
      }),
      field("Which", text("change-role-other", { placeholder: "a role this build knows" }), {
        id: "change-role-other-field",
        hidden: true,
      }),
    ),
    preview("change-preview"),
    row("default", button("change", "Run it", "primary")),
  );

const removeOne = (): Node =>
  pane(
    paneHead("Remove one", status("remove-status")),
    warning(
      el("strong", {}, "This does not come back."),
      " The user's grants go with them, and a new user of the same name is a " +
        "different user with none of them. If it is the last owner of the whole " +
        "node, nothing can let anybody back in — a closed store has no door from " +
        "outside, and the way back is a restore from backup.",
    ),
    note(
      "You may only remove somebody in the tenancy you administer. The node refuses " +
        "the rest, and what it says is shown here as it said it.",
    ),
    row(
      "default",
      field("Who", text("remove-name", { placeholder: "ada" })),
      field("Type the name again", text("remove-confirm", { placeholder: "ada" })),
    ),
    preview("remove-preview"),
    row("default", button("remove", "Remove", "default", { disabled: true })),
    el("hr", { class: "between" }),
    paneHead("Your own password", status("mine-status")),
    note(
      "The pane above is for somebody else's credential and needs an owner. This " +
        "one is yours, and any role may use it — a viewer whose password may have " +
        "leaked should not have to ask an owner to choose them a new one.",
    ),
    row(
      "default",
      field("Current", secret("mine-current", "current-password")),
      field("New", secret("mine-new", "new-password")),
      field("New again", secret("mine-again", "new-password")),
    ),
    row("default", button("mine", "Change it", "default", { disabled: true })),
    note(
      "The current one is asked for because being signed in is not proof of a " +
        "password — otherwise a token copied off this connection would be a way to " +
        "take the account rather than borrow it. Changing it ends every session you " +
        "hold, this one included, so you will be asked to sign in again.",
    ),
  );

const users = (): Node =>
  panel(
    DESTINATIONS[1]!,
    false,
    split(whoThereIs(), defineOne()),
    split(changeOne(), removeOne()),
  );

const node = (): Node =>
  panel(
    DESTINATIONS[2]!,
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

const cluster = (): Node =>
  panel(
    DESTINATIONS[3]!,
    false,
    pane(paneHead("Membership", status("cluster-status")), answer("cluster-facts", "small")),
    pane(
      paneHead("What is here, and what is not"),
      note(
        "What the pane above shows is what this node itself knows: its own " +
          "membership, the peers it has been told about, and the addresses it answers " +
          "on. It is read from the node, not from anything standing beside it.",
      ),
      note(
        "Membership is a record and not a control plane: a node learns its peers " +
          "through the same log it replicates data with, so there is nothing to stand " +
          "up beside the database. A namespace declares how many copies the cluster " +
          "keeps, and a peer's subscription decides which of them it receives. A " +
          "leader holds a renewable lease granted by a majority and stops writing " +
          "before that lease expires, measured on elapsed time rather than on the " +
          "clock. A read may name the staleness it will accept, and a bound tighter " +
          "than the cluster can know about itself is refused with the floor named. A " +
          "write sent to the wrong node comes back as a redirect carrying the " +
          "endpoint, the node and the epoch it was decided under. A divergence is " +
          "refused at the first divergent record and counted, never silently ranked.",
      ),
      warning(
        el("strong", {}, "There is no sharding."),
        " Every node that holds a namespace holds all of it; one dataset is not " +
          "split across machines by key. A namespace larger than one machine is the " +
          "case this engine does not serve.",
      ),
      note(
        el("strong", {}, "This pane is not yet a cluster surface."),
        " Nothing here manages anything: roles, failover, standby nodes, data " +
          "placement and a live map of cluster state are being designed, and are " +
          "deliberately absent rather than stubbed. Nor does it draw lag or " +
          "leadership for other nodes — nothing pulls a replica forward on a timer, " +
          "so a node that is not writing has no last collection its copy could be " +
          "measured from, and a number invented here would be the dashboard drawn " +
          "ahead of the engine that makes the rest of this console untrustworthy.",
      ),
    ),
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
        el("main", {}, query(), users(), node(), cluster()),
        el("script", { src: "/console.js" }),
        el("script", { src: "/sections.js" }),
      ),
    ),
  );
