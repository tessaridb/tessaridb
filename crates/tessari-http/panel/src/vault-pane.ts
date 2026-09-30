//! Vault — the store's key and one vault's, opened, closed and rekeyed.
//!
//! The markup only; what the buttons send is in `vault.ts`. Every passphrase
//! field is `type="password"`, so the console never keeps or restores it.

import { el, type Node } from "./html.js";
import { to } from "./destinations.js";
import { answer, button, field, note, pane, paneHead, panel, row, says, secret, status, text } from "./ui.js";

export const vault = (): Node =>
  panel(
    to("vault"),
    false,
    paneHead("The store's key", status("vault-store-status")),
    pane(
      note(
        "Opens every vault declared without a passphrase of its own for the node's period (",
        el("code", {}, "--unseal-for"),
        ", ten minutes by default), then closes by itself. Store-wide operators only.",
      ),
      answer("vault-store-answer", "small"),
      row("default", field("Passphrase", secret("vault-store-passphrase", "current-password"))),
      row(
        "default",
        button("vault-store-unseal", "Unseal", "primary", { disabled: true }),
        button("vault-store-seal", "Seal", "default"),
        button("vault-store-refresh", "Refresh", "default"),
      ),
      row(
        "default",
        field("Current passphrase", secret("vault-store-current", "current-password")),
        field("New passphrase", secret("vault-store-new", "new-password")),
      ),
      row("default", button("vault-store-change", "Change passphrase", "default", { disabled: true })),
    ),
    pane(
      paneHead("One vault", status("vault-one-status")),
      note(
        "A vault declared with ",
        el("code", {}, "PASSPHRASE '…'"),
        " opens with its own passphrase. Readers of its database may open or close it; changing " +
          "the passphrase needs manage there.",
      ),
      row(
        "default",
        field("Namespace", text("vault-one-namespace", { spellcheck: false, autocomplete: "off" })),
        field("Database", text("vault-one-database", { spellcheck: false, autocomplete: "off" })),
        field("Vault", text("vault-one-name", { spellcheck: false, autocomplete: "off" })),
      ),
      says("vault-one-says"),
      answer("vault-one-answer", "small"),
      row("default", field("Passphrase", secret("vault-one-passphrase", "current-password"))),
      row(
        "default",
        button("vault-one-show", "Status", "default", { disabled: true }),
        button("vault-one-unseal", "Unseal", "primary", { disabled: true }),
        button("vault-one-seal", "Seal", "default", { disabled: true }),
      ),
      row(
        "default",
        field("Current passphrase", secret("vault-one-current", "current-password")),
        field("New passphrase", secret("vault-one-new", "new-password")),
      ),
      row("default", button("vault-one-change", "Change passphrase", "default", { disabled: true })),
    ),
    pane(
      paneHead("Records in a vault", status("vault-rec-status")),
      note(
        "Ids come a page at a time with no value beside them. A reveal is recorded by the store before it answers; " +
          "the plaintext stays on this page until you hide it or reveal another.",
      ),
      row(
        "default",
        field("Namespace", text("vault-rec-namespace", { spellcheck: false, autocomplete: "off" })),
        field("Database", text("vault-rec-database", { spellcheck: false, autocomplete: "off" })),
        button("vault-rec-list", "Vaults here", "default"),
      ),
      el("div", { id: "vault-rec-vaults" }),
      row(
        "default",
        field("Vault", text("vault-rec-name", { spellcheck: false, autocomplete: "off" })),
        button("vault-rec-records", "Records", "primary"),
        button("vault-rec-more", "Next page", "default", { disabled: true }),
      ),
      el("div", { id: "vault-rec-ids" }),
      el("div", { id: "vault-rec-shown", "aria-live": "polite" }),
      row("default", button("vault-rec-hide", "Hide the revealed values", "quiet", { disabled: true })),
      row(
        "default",
        field("Record id", text("vault-rec-id", { spellcheck: false, autocomplete: "off" })),
        field("Field", text("vault-rec-field", { spellcheck: false, autocomplete: "off" })),
        field("Value", secret("vault-rec-value", "new-password")),
        button("vault-rec-write", "Write", "default", { disabled: true }),
      ),
      row(
        "default",
        field("Read by (optional)", text("vault-rec-actor", { spellcheck: false, autocomplete: "off" })),
        button("vault-rec-audit", "Audit trail", "default"),
      ),
      el("div", { id: "vault-rec-trail" }),
    ),
  );
