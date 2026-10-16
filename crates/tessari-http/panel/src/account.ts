//! Who the reader is: the sign-in page they meet first, the account menu at the
//! sidebar's foot, and the dialog that changes their own password.
//!
//! The behaviour lives in `gate.ts`, `session.ts` and `password.ts`. This file draws.

import { el, type Node } from "./html.js";
import { behindDisclosure, button, dialog, field, note, row, secret, status } from "./ui.js";

/**
 * The account menu at the sidebar's foot: who this is, their own password, and
 * signing out. Signing IN is not here — it is the page a visitor meets first.
 */
export const identity = (): Node =>
  el(
    "details",
    { class: "identity" },
    el("summary", {}, el("span", { id: "signed-in" }, "not signed in")),
    el(
      "div",
      { class: "sheet" },
      row(
        "tight",
        button("mine-open", "Your password…", "quiet"),
        button("sign-out", "Sign out", "quiet"),
      ),
    ),
  );

/**
 * The page a visitor meets before the console: centred, and nothing else on it.
 *
 * A form so Enter signs in from either field. It never navigates — the gate
 * stops the submit and the button's own handler does the signing in.
 */
export const gate = (): Node =>
  el(
    "section",
    { id: "gate", class: "gate", hidden: true, "aria-labelledby": "gate-title" },
    el(
      "form",
      { id: "gate-form", class: "gate-card" },
      el(
        "div",
        { class: "gate-brand" },
        el("img", { class: "logo", src: "/favicon.svg", alt: "", width: 40, height: 40 }),
        el("h1", { id: "gate-title" }, "Sign in to TessariDB"),
        el("p", { id: "gate-where", class: "faint" }, "this node"),
      ),
      field("User", el("input", { id: "user", type: "text", autocomplete: "username" })),
      field("Password", secret("password", "current-password")),
      button("sign-in", "Sign in", "primary", { type: "submit" }),
      status("identity-status"),
      // Rewritten on load from how this page arrived: a node given a client
      // certificate serves the console over TLS, and one that was not still
      // sends the password as it was typed.
      el(
        "p",
        { id: "transport-says", class: "note warn" },
        "Credentials travel as typed unless this page arrived over https.",
      ),
      behindDisclosure(
        "What signing in keeps",
        note(
          "The password is checked once. What this page keeps afterwards is a token, " +
            "held for as long as the tab is open and no longer.",
        ),
      ),
    ),
  );

/** Your own password, opened from the account menu. Any role may use it. */
export const mineDialog = (): Node =>
  dialog(
    "mine-dialog",
    "Your password",
    row(
      "default",
      field("Current", secret("mine-current", "current-password")),
      field("New", secret("mine-new", "new-password")),
      field("New again", secret("mine-again", "new-password")),
    ),
    // The CONSEQUENCE stays in the flow and the REASON goes behind the
    // disclosure. Somebody about to press this needs to know they will be signed
    // out; why the current password is asked for is a thing they can look up.
    note("Changing it ends every session you hold, this one included."),
    behindDisclosure(
      "Why the current one is asked for",
      note(
        "Being signed in is not proof of a password — otherwise a token copied off " +
          "this connection would be a way to take the account rather than borrow it.",
      ),
    ),
    row("spread", button("mine", "Change it", "primary", { disabled: true }), status("mine-status")),
  );

