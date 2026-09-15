//! Access — who exists, and the four panes that decide what they may reach.
//!
//! The screen as a value. It was inside `page.ts` until the page reached 458
//! lines, which is a god-file by the project's own rule; what is here is one
//! screen and nothing else, and `page.ts` now assembles screens rather than
//! containing them.
//!
//! The behaviour lives in `users.ts` and `user-forms.ts`. This file draws.

import { el, type Node } from "./html.js";
import { to } from "./destinations.js";
import {
  answer, behindDisclosure, button, choose, field, note, pane, paneHead, panel,
  preview, ROLES, row, says, secret, split, status, text, warning,
} from "./ui.js";

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
    row(
      "spread",
      field("Filter", text("user-filter", { placeholder: "part of a name" })),
      status("user-count"),
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
    says("define-preview"),
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
    row(
      "default",
      field("Why", text("change-why", { placeholder: "rotating a credential that leaked" }), {
        class: "wide",
      }),
    ),
    says("change-preview"),
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
    row(
      "default",
      field("Why", text("remove-why", { placeholder: "left the team on Friday" }), {
        class: "wide",
      }),
    ),
    says("remove-radius"),
    behindDisclosure("The statement this will send", preview("remove-preview")),
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

/**
 * Giving and taking away what one account reaches.
 *
 * Its own pane rather than a drawer over a user detail, which is the shape the
 * brief sketched: there is no separate user-detail SURFACE to put a drawer over
 * — the detail is a pane on this screen — and a drawer over a pane would be a
 * sheet over the thing it belongs to. Recorded as a difference from the brief
 * rather than taken silently.
 */
const giveOrTake = (): Node =>
  pane(
    paneHead("Give or take away", status("grant-status")),
    note(
      "A grant is not only an addition. A user with no table grants is governed " +
        "by their role; a user with one reaches exactly what they were granted — " +
        "so the first grant narrows, and taking the last one away widens. The line " +
        "under the form says which of the two you are about to do.",
    ),
    row(
      "default",
      field("Who", text("grant-who", { placeholder: "ada" })),
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
    row("default", button("grant-apply", "Run it", "primary")),
  );

export const access = (): Node =>
  panel(
    to("access"),
    false,
    split(whoThereIs(), defineOne()),
    split(changeOne(), removeOne()),
    giveOrTake(),
  );
