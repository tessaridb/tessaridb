//! Trust on the cluster tab: what this node presents, what the cluster refuses,
//! who may join, and the failover periods (ADR-0108 D6, D9; DEFINE FAILOVER).
//!
//! Everything drawn comes from the `INFO FOR NODE` answer the map was drawn
//! from, so the panes and the map can never describe two different moments.
//!
//! # The irreversible one asks for the value again
//!
//! A revocation reaches every node and cannot be taken back, so the button stays
//! dead until the first eight digits are typed a second time, and the sentence
//! above it says when the fingerprint is this node's own — revoking that cuts
//! this node off from its own cluster, which is a thing to be told before, not
//! after.

import { valueOf } from "./api.js";
import { at, clear, disable, hide, made, say, setValue, trimmed, write } from "./dom.js";
import { told } from "./session.js";
import { hereAgain } from "./tabs.js";
import { aDuration, aName } from "./topic-names.js";

const SCREEN = "Cluster · trust";
const FINGERPRINT = /^[0-9a-f]{64}$/;
const DAY_MS = 86_400_000;
const PERIODS = ["awareness", "collection", "round", "campaign", "lease"] as const;

/** One certificate this node presents, as `certificates[]` carries it. */
interface Shown {
  readonly surface?: string;
  readonly fingerprint?: string;
  readonly expires?: unknown;
}

/** A membership row, as far as trust reads it. */
interface Row {
  readonly name?: string;
  readonly node?: string | null;
  readonly fingerprint?: string | null;
  readonly join_expires_ms?: unknown;
}

/** The part of `INFO FOR NODE` these panes read. */
export interface Trusted {
  readonly certificates?: readonly Shown[];
  readonly cluster?: {
    readonly peers?: readonly Row[];
    readonly revoked?: readonly string[];
    readonly tombstoned?: readonly string[];
    readonly failover?: Readonly<Record<string, unknown>> | null;
  };
}

let mine: readonly Shown[] = [];
let rows: readonly Row[] = [];

/** `0f1e…` from any spelling a certificate tool prints, or `null`. */
export function aFingerprint(text: string): string | null {
  const plain = text.replace(/:/g, "").toLowerCase();
  return FINGERPRINT.test(plain) ? plain : null;
}

/** How long until `expires`, in whole days, or `null` when it is not a date. */
function daysLeft(expires: unknown): number | null {
  const at_ = typeof expires === "string" ? Date.parse(expires) : Number.NaN;
  return Number.isNaN(at_) ? null : Math.floor((at_ - Date.now()) / DAY_MS);
}

/** A list of values, or the sentence that says it is empty. */
function listed(where: string, values: readonly string[], empty: string): void {
  clear(where);
  if (values.length === 0) {
    const none = made("p", "faint");
    none.textContent = empty;
    at(where).appendChild(none);
    return;
  }
  const list = made("ul");
  for (const one of values) {
    const item = made("li");
    const code = made("code");
    code.textContent = one;
    item.appendChild(code);
    list.appendChild(item);
  }
  at(where).appendChild(list);
}

function drawMine(): void {
  clear("trust-mine");
  if (mine.length === 0) {
    const none = made("p", "faint");
    none.textContent =
      "This node serves its clients in the clear: --tls-cert and --tls-key would encrypt " +
      "them, and --require-client-tls refuses to start without them.";
    at("trust-mine").appendChild(none);
    return;
  }
  for (const shown of mine) {
    const line = made("p");
    const left = daysLeft(shown.expires);
    const when =
      left === null
        ? "its expiry could not be read"
        : left < 0
          ? `EXPIRED ${-left} day(s) ago — every handshake refuses it`
          : `expires in ${left} day(s) (${told(shown.expires)})`;
    line.textContent = `${shown.surface ?? "?"} — ${when}: `;
    const code = made("code");
    code.textContent = shown.fingerprint ?? "";
    line.appendChild(code);
    if (left !== null && left < 14) {
      line.className = "note warn";
    }
    at("trust-mine").appendChild(line);
  }
}

function drawJoin(): void {
  const select = at("join-row");
  clear("join-row");
  const open = rows.filter((row) => (row.node ?? null) === null && typeof row.name === "string");
  if (open.length === 0) {
    hide("join-token", true);
  }
  for (const row of open) {
    const option = made("option");
    option.value = row.name ?? "";
    const until = row.join_expires_ms;
    option.textContent =
      (row.name ?? "") +
      (typeof until === "number" ? ` (a token waits until ${new Date(until).toLocaleTimeString()})` : "");
    select.appendChild(option);
  }
  shapeJoin();
}

function drawFailover(held: Readonly<Record<string, unknown>> | null | undefined): void {
  write(
    "failover-held",
    held === null || held === undefined
      ? "Nobody has set a policy: every node runs the built-in periods shown as placeholders."
      : "Set: " + PERIODS.map((clause) => `${clause.toUpperCase()} ${told(held[clause])}`).join(", ") +
          ` (epoch ${told(held["epoch"])}, version ${told(held["version"])}).`,
  );
}

/** Draw the trust panes from the answer the map was drawn from. */
export function draw(seen: Trusted): void {
  mine = seen.certificates ?? [];
  rows = seen.cluster?.peers ?? [];
  drawMine();
  listed("trust-revoked", seen.cluster?.revoked ?? [], "No certificate is refused.");
  listed("trust-removed", seen.cluster?.tombstoned ?? [], "No node has been removed.");
  drawJoin();
  drawFailover(seen.cluster?.failover);
  shapeRevoke();
  shapeFailover();
}

/** The revocation, or the sentence that says what is missing. */
function revocation(): { readonly statement: string; readonly says: string } | { readonly missing: string } {
  const fingerprint = aFingerprint(trimmed("revoke-fingerprint"));
  if (fingerprint === null) {
    return { missing: "a fingerprint: 64 hexadecimal digits, with or without colons" };
  }
  if (trimmed("revoke-confirm").toLowerCase() !== fingerprint.slice(0, 8)) {
    return { missing: `type ${fingerprint.slice(0, 8)} again to confirm` };
  }
  const own = mine.find((shown) => shown.fingerprint === fingerprint);
  const pinned = rows.find((row) => row.fingerprint === fingerprint);
  return {
    statement: `REVOKE CERTIFICATE '${fingerprint}';`,
    says:
      (own !== undefined
        ? `This is the certificate THIS node presents on its ${own.surface ?? ""} surface — ` +
          "every peer will refuse this node until it is given a new one. "
        : pinned !== undefined
          ? `This is the certificate pinned to ${pinned.name ?? "a row"}. `
          : "") + "Every node refuses it, in both directions, for good.",
  };
}

function shapeRevoke(): void {
  const composed = revocation();
  disable("revoke-apply", !("statement" in composed));
  say("revoke-says", "statement" in composed ? composed.says : composed.missing);
}

function shapeJoin(): void {
  const name = aName(trimmed("join-row"));
  disable("join-apply", name === null);
  say(
    "join-says",
    name === null
      ? "No row waits for a node: declare one without a node id or a fingerprint first."
      : `Issues a token that binds ${name} to the first node offering it, for ${trimmed("join-life")}.`,
  );
}

/** The `DEFINE FAILOVER` the form describes, or what is missing from it. */
function policy(): { readonly statement: string } | { readonly missing: string } {
  const said: string[] = [];
  for (const clause of PERIODS) {
    const period = aDuration(trimmed(`failover-${clause}`));
    if (period === null) {
      return { missing: `${clause.toUpperCase()} needs a duration such as 200ms or 1s — every clause is required` };
    }
    said.push(`${clause.toUpperCase()} ${period}`);
  }
  return { statement: `DEFINE FAILOVER ${said.join(" ")};` };
}

function shapeFailover(): void {
  const composed = policy();
  disable("failover-apply", !("statement" in composed));
  say(
    "failover-says",
    "statement" in composed
      ? `Replaces the policy on every node: ${composed.statement} The node refuses a set whose periods do not hold together, and says which.`
      : composed.missing,
  );
}

async function send(statement: string, status: string, why?: string): Promise<boolean> {
  say(status, "sending…");
  try {
    const answered = await valueOf(statement, SCREEN, why);
    say(status, "done");
    return answered !== null;
  } catch (failure) {
    say(status, told(failure), true);
    return false;
  }
}

export function wire(): void {
  for (const id of ["revoke-fingerprint", "revoke-confirm"]) {
    at(id).addEventListener("input", shapeRevoke);
  }
  for (const id of ["join-row", "join-life"]) {
    at(id).addEventListener("change", shapeJoin);
  }
  for (const clause of PERIODS) {
    at(`failover-${clause}`).addEventListener("input", shapeFailover);
  }

  at("revoke-apply").addEventListener("click", async () => {
    const composed = revocation();
    if (!("statement" in composed)) {
      return;
    }
    if (await send(composed.statement, "revoke-status", trimmed("revoke-why") || undefined)) {
      setValue("revoke-confirm", "");
      setValue("revoke-fingerprint", "");
      hereAgain();
    }
  });

  at("join-apply").addEventListener("click", async () => {
    const name = aName(trimmed("join-row"));
    const life = aDuration(trimmed("join-life"));
    if (name === null || life === null) {
      return;
    }
    say("join-status", "sending…");
    hide("join-token", true);
    try {
      const answered = await valueOf(`CREATE JOIN TOKEN FOR REPLICA ${name} EXPIRES ${life};`, SCREEN);
      const token = answered !== null && answered.kind === "value" ? answered.value : null;
      if (typeof token !== "string") {
        say("join-status", "the node answered without a token", true);
        return;
      }
      // Shown here and nowhere else: the statement log keeps a placeholder.
      write("join-token", `--join-token ${token}\n\nShown once. The node keeps only its digest.`);
      hide("join-token", false);
      say("join-status", "issued");
      hereAgain();
    } catch (failure) {
      say("join-status", told(failure), true);
    }
  });

  at("failover-apply").addEventListener("click", async () => {
    const composed = policy();
    if ("statement" in composed && (await send(composed.statement, "failover-status"))) {
      hereAgain();
    }
  });
}
