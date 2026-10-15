//! Access — who exists, with what may be done to each beside them.
//!
//! One list, and every action on an account in a dialog of its own: create,
//! look at, change a password, change a role, give or take away, remove. The
//! forms are the ones the screen always had; what changed is that they stand
//! behind the row they act on instead of stacked under the list, keyed by a name
//! typed again by hand.
//!
//! The behaviour lives in `users.ts`, `user-detail.ts`, `user-actions.ts`,
//! `user-forms.ts` and `grants.ts`. This file draws.

import { el, type Node } from "./html.js";
import { to } from "./destinations.js";
import {
  answer, behindDisclosure, button, choose, dialog, field, note, pane, paneHead, panel,
  preview, ROLES, row, says, secret, status, text, warning,
} from "./ui.js";

/** The name an action is about, shown and not typed: it came from the row. */
const who = (id: string): Node => field("Who", text(id, { readonly: true }));

const users = (): Node =>
  pane(
    paneHead(
      "Users",
      row(
        "tight",
        status("user-status"),
        button("list", "Refresh", "quiet"),
        button("new-user", "New user…", "primary"),
      ),
    ),
    note("What you are shown is the tenancy you administer."),
    behindDisclosure(
      "What that means for a short list",
      note(
        "An owner of a space sees that space; an ",
        el("strong", {}, "admin"),
        " — an owner with no space at all — sees the whole node. It refuses rather " +
          "than narrowing, so a short list is a short tenancy and never a filtered one.",
      ),
    ),
    row(
      "spread",
      field("Filter", text("user-filter", { placeholder: "part of a name" })),
      status("user-count"),
    ),
    answer("user-list", "small"),
  );

/** One account looked at: what it is, what it holds, and taking any of it away. */
const userDialog = (): Node =>
  dialog(
    "user-dialog",
    "User",
    status("user-detail-status"),
    answer("user-answer", "small"),
    el("div", { id: "user-holds" }),
    row(
      "tight",
      button("user-grant", "Give…", "default"),
      button("user-password", "Password…", "quiet"),
      button("user-role", "Role…", "quiet"),
      button("user-remove", "Remove…", "quiet"),
    ),
  );

const defineDialog = (): Node =>
  dialog(
    "define-dialog",
    "New user",
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
    says("define-preview"),
    row("spread", button("define", "Create", "primary"), status("define-status")),
  );

const changeDialog = (): Node =>
  dialog(
    "change-dialog",
    "Change",
    behindDisclosure(
      "Why a space is not offered",
      note(
        "A password rotation does not touch a role, and a role correction does not " +
          "invalidate a password. Widening somebody's reach is the one change an " +
          "owner of a part could use to reach the whole, so it is not offered here " +
          "at all.",
      ),
    ),
    row(
      "default",
      who("change-name"),
      field(
        "What ",
        choose("change-what", [
          { value: "password", label: "password" },
          { value: "role", label: "role" },
        ]),
        { id: "change-what-field", hidden: true },
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
    row(
      "default",
      field("Why", text("change-why", { placeholder: "rotating a credential that leaked" }), {
        class: "wide",
      }),
    ),
    says("change-preview"),
    row("spread", button("change", "Change", "primary"), status("change-status")),
  );

const removeDialog = (): Node =>
  dialog(
    "remove-dialog",
    "Remove",
    warning(
      el("strong", {}, "This does not come back."),
      " The user's grants go with them, and a new user of the same name is a " +
        "different user with none of them. If it is the last owner of the whole " +
        "node, nothing can let anybody back in — a closed store has no door from " +
        "outside, and the way back is a restore from backup.",
    ),
    row(
      "default",
      who("remove-name"),
      field("Type the name again", text("remove-confirm", { placeholder: "the name above" })),
    ),
    row(
      "default",
      field("Why", text("remove-why", { placeholder: "left the team on Friday" }), {
        class: "wide",
      }),
    ),
    says("remove-radius"),
    behindDisclosure("The statement this will send", preview("remove-preview")),
    row(
      "spread",
      button("remove", "Remove", "default", { disabled: true, class: "danger" }),
      status("remove-status"),
    ),
  );

/**
 * Giving and taking away what one account reaches.
 *
 * The account comes from the row or the detail it was opened from, so the
 * dialog asks only what and where — and says, before the button, which of the
 * two a grant is this time.
 */
const grantDialog = (): Node =>
  dialog(
    "grant-dialog",
    "Give or take away",
    behindDisclosure(
      "Why a grant can narrow",
      note(
        "A user with no table grants is governed by their role; a user with one " +
          "reaches exactly what they were granted. So the first grant narrows, and " +
          "taking the last one away widens them back to the role — which the node " +
          "refuses.",
      ),
    ),
    row(
      "default",
      who("grant-who"),
      field(
        "Direction ",
        choose("grant-direction", [
          { value: "give", label: "give", chosen: true },
          { value: "take", label: "take away" },
        ]),
      ),
      field(
        "On ",
        choose("grant-reach", [
          { value: "table", label: "a table", chosen: true },
          { value: "namespace", label: "a namespace" },
          { value: "database", label: "a database" },
          { value: "store", label: "the whole store" },
        ]),
      ),
      field("Named", text("grant-name", { placeholder: "app.main.orders" }), {
        id: "grant-name-field",
      }),
      field("What", text("grant-what", { placeholder: "read, write" })),
    ),
    row(
      "default",
      field("Why", text("grant-why", { placeholder: "joining the billing rota" }), {
        class: "wide",
      }),
    ),
    says("grant-says"),
    row("spread", button("grant-apply", "Apply", "primary"), status("grant-status")),
  );

export const access = (): Node => panel(to("access"), false, users());

/**
 * The screen's dialogs, placed by the page outside every panel: a dialog inside
 * a hidden panel is not drawn even when it is open, and the owner's dialog is
 * opened from the open-store banner on whichever screen is showing.
 */
export const accessDialogs = (): Node[] => [
  userDialog(),
  defineDialog(),
  changeDialog(),
  removeDialog(),
  grantDialog(),
];
