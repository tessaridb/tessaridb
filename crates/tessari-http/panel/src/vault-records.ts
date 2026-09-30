//! The Vault screen's records: which vaults a database holds, a vault's record
//! ids a page at a time, a reveal on click, a write of one field, and the audit
//! trail — all through `POST /script`, as any client would send them.
//!
//! A namespace, database, vault, field and actor are grammar, so each passes
//! `aName()` and is written into the text. A record id and a written value are
//! values, so they are BOUND: the statement log keeps the script and never its
//! parameters, which is what keeps a written secret out of it. Revealed values
//! live in one element on this page and nowhere else — no storage, no log line —
//! and are removed by Hide, by the next reveal, and by listing again.

import { held, Unreachable, valueOf } from "./api.js";
import { at, clear, disable, made, say, setValue, shown, trimmed, value } from "./dom.js";
import { told } from "./session.js";
import { aName, tenancy } from "./topic-names.js";
import { quoted } from "./user-forms.js";

const SCREEN = "Vault";
const PAGE = 50;

/** Where the pane points: a tenancy and, when named, a vault. */
interface Place {
  readonly tenancy: string;
  readonly vault: string | null;
}

let after: string | null = null;

/** The checked tenancy and vault, or the sentence that says which name is not one. */
function place(needsVault: boolean): Place | { readonly missing: string } {
  const namespace = aName(trimmed("vault-rec-namespace"));
  const database = aName(trimmed("vault-rec-database"));
  if (namespace === null) return { missing: "name the namespace" };
  if (database === null) return { missing: "name the database" };
  const typed = trimmed("vault-rec-name");
  const vault = typed === "" ? null : aName(typed);
  if (needsVault && vault === null) return { missing: "name the vault" };
  return { tenancy: tenancy(namespace, database), vault };
}

/** A record id as the TessariQL source of its literal, or `null` for one this pane cannot bind. */
function literal(id: unknown): string | null {
  if (typeof id === "string") return quoted(id);
  if (typeof id === "number" && Number.isSafeInteger(id)) return String(id);
  return null;
}

function failed(failure: unknown): void {
  const words = told(failure);
  say("vault-rec-status", failure instanceof Unreachable ? "the node did not answer — " + words : words, true);
}

function hideRevealed(): void {
  clear("vault-rec-shown");
  disable("vault-rec-hide", true);
}

async function listVaults(): Promise<void> {
  const where = place(false);
  if ("missing" in where) return say("vault-rec-status", where.missing, true);
  clear("vault-rec-vaults");
  try {
    const report = held(await valueOf(`${where.tenancy}INFO FOR DATABASE;`, SCREEN));
    const names = Array.isArray(report?.vaults) ? report.vaults : [];
    const list = made("ul");
    for (const each of names) {
      const name = typeof each === "string" ? each : (each as { name?: unknown } | null)?.name;
      if (typeof name !== "string") continue;
      const pick = made("button", "quiet");
      pick.type = "button";
      pick.textContent = name;
      pick.addEventListener("click", () => {
        setValue("vault-rec-name", name);
        void listRecords(true);
      });
      const item = made("li");
      item.appendChild(pick);
      list.appendChild(item);
    }
    at("vault-rec-vaults").appendChild(list);
    say("vault-rec-status", names.length === 0 ? "this database holds no vault" : `${names.length} vault(s)`);
  } catch (failure) {
    failed(failure);
  }
}

async function listRecords(first: boolean): Promise<void> {
  const where = place(true);
  if ("missing" in where || where.vault === null) {
    return say("vault-rec-status", "missing" in where ? where.missing : "name the vault", true);
  }
  if (first) after = null;
  hideRevealed();
  const vault = where.vault;
  const from = after === null ? "" : ` AFTER ${vault}:$after`;
  const bound = after === null ? undefined : { after };
  try {
    const report = held(
      await valueOf(`${where.tenancy}INFO FOR VAULT ${vault} RECORDS${from} LIMIT ${PAGE};`, SCREEN, undefined, bound),
    );
    const ids = Array.isArray(report?.records) ? report.records : [];
    clear("vault-rec-ids");
    const list = made("ul");
    for (const id of ids) {
      const item = made("li");
      const bindable = literal(id);
      const open = made("button", "quiet");
      open.type = "button";
      open.textContent = typeof id === "string" ? id : JSON.stringify(id);
      open.disabled = bindable === null;
      if (bindable !== null) open.addEventListener("click", () => void reveal(where.tenancy, vault, bindable));
      item.appendChild(open);
      list.appendChild(item);
    }
    at("vault-rec-ids").appendChild(list);
    const next = report?.next;
    after = next === undefined || next === null ? null : literal(next);
    disable("vault-rec-more", after === null);
    say("vault-rec-status", ids.length === 0 ? "no records on this page" : `${ids.length} id(s) — choose one to reveal`);
  } catch (failure) {
    failed(failure);
  }
}

async function reveal(where: string, vault: string, id: string): Promise<void> {
  hideRevealed();
  try {
    const opened = held(await valueOf(`${where}REVEAL * FROM ${vault}:$id;`, SCREEN, undefined, { id }));
    at("vault-rec-shown").appendChild(shown(opened ?? {}));
    disable("vault-rec-hide", false);
    say("vault-rec-status", "revealed — the store recorded this read");
  } catch (failure) {
    failed(failure);
  }
}

async function writeField(): Promise<void> {
  const where = place(true);
  const field = aName(trimmed("vault-rec-field"));
  const id = trimmed("vault-rec-id");
  if ("missing" in where || where.vault === null) {
    return say("vault-rec-status", "missing" in where ? where.missing : "name the vault", true);
  }
  if (field === null) return say("vault-rec-status", "name the field", true);
  if (id === "") return say("vault-rec-status", "name the record id", true);
  // Taken out of the field before it is sent, so it does not stay in the page.
  const secret = value("vault-rec-value");
  setValue("vault-rec-value", "");
  shape();
  try {
    await valueOf(
      `${where.tenancy}UPSERT ${where.vault}:$id MERGE { '${field}': $value };`,
      SCREEN,
      undefined,
      { id: quoted(id), value: quoted(secret) },
    );
    say("vault-rec-status", `wrote ${field} on ${id}`);
  } catch (failure) {
    failed(failure);
  }
}

async function auditTrail(): Promise<void> {
  const where = place(false);
  if ("missing" in where) return say("vault-rec-status", where.missing, true);
  const typed = trimmed("vault-rec-actor");
  const actor = typed === "" ? null : aName(typed);
  if (typed !== "" && actor === null) return say("vault-rec-status", `${typed} is not a user name`, true);
  clear("vault-rec-trail");
  try {
    const by = actor === null ? "" : ` BY ${actor}`;
    const report = held(await valueOf(`${where.tenancy}INFO FOR AUDIT${by};`, SCREEN));
    at("vault-rec-trail").appendChild(shown(report?.audit ?? []));
    say("vault-rec-status", "the audit trail, oldest first");
  } catch (failure) {
    failed(failure);
  }
}

function shape(): void {
  disable(
    "vault-rec-write",
    trimmed("vault-rec-id") === "" || trimmed("vault-rec-field") === "" || value("vault-rec-value") === "",
  );
}

export function wireRecords(): void {
  at("vault-rec-list").addEventListener("click", () => void listVaults());
  at("vault-rec-records").addEventListener("click", () => void listRecords(true));
  at("vault-rec-more").addEventListener("click", () => void listRecords(false));
  at("vault-rec-hide").addEventListener("click", hideRevealed);
  at("vault-rec-write").addEventListener("click", () => void writeField());
  at("vault-rec-audit").addEventListener("click", () => void auditTrail());
  for (const id of ["vault-rec-id", "vault-rec-field", "vault-rec-value"]) {
    at(id).addEventListener("input", shape);
  }
  shape();
}
