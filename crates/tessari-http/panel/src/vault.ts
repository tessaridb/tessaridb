//! The Vault screen: the store's key over `/vault`, and one vault's over
//! `/vault/{namespace}/{database}/{vault}` (ADR-0092, ADR-0093).
//!
//! The passphrase travels as the body of its own route and never as a statement,
//! so it is in no script, no statement log line and no saved form: the log
//! records `METHOD path`, a passphrase field is `type="password"`, and each one is
//! emptied as soon as its request has gone. A vault's three names are grammar
//! in the path, so each passes `aName()` first and a name that fails is refused
//! here, before anything is sent.

import { Unreachable, route } from "./api.js";
import { at, disable, say, setValue, trimmed, value, write } from "./dom.js";
import { told } from "./session.js";
import { aName } from "./topic-names.js";
import { wireRecords } from "./vault-records.js";

const SCREEN = "Vault";

/** The status every vault route answers with, as the console shows it. */
function shown(body: unknown): string {
  if (typeof body !== "object" || body === null) return "";
  const answered = body as Record<string, unknown>;
  const state = typeof answered.state === "string" ? answered.state : "unknown";
  const lines = [state];
  if (typeof answered.seals_at === "string") {
    lines.push(`seals at ${new Date(answered.seals_at).toLocaleString()}`);
  }
  if (typeof answered.unseal_for === "string") lines.push(`an unseal lasts ${answered.unseal_for}`);
  if (typeof answered.custody === "string") {
    lines.push(answered.custody === "own" ? "opens with its own passphrase" : "opens with the store's passphrase");
  }
  if (answered.initialised === true) lines.push("this unseal set the store's first passphrase");
  return lines.join("\n");
}

/** The node's own words for a refusal, or the status of a success. */
function refusal(body: unknown): string {
  if (typeof body === "object" && body !== null && typeof (body as { error?: unknown }).error === "string") {
    return (body as { error: string }).error;
  }
  return typeof body === "string" && body !== "" ? body : "refused";
}

async function act(
  method: "GET" | "POST",
  path: string,
  statusId: string,
  answerId: string,
  body?: string,
): Promise<void> {
  say(statusId, "working…");
  try {
    const answered = await route(method, path, SCREEN, body);
    if (answered.status === 200) {
      write(answerId, shown(answered.body));
      say(statusId, "done");
    } else {
      // 429 is a throttle: it means wait, not wrong.
      const words = answered.status === 429 ? "too many wrong passphrases — wait and try again" : refusal(answered.body);
      say(statusId, words, true);
    }
  } catch (failure) {
    const words = told(failure);
    say(statusId, failure instanceof Unreachable ? "the node did not answer — " + words : words, true);
  }
}

/** Take a passphrase out of its field, so it does not stay in the page. */
function spent(id: string): string {
  const held = value(id);
  setValue(id, "");
  return held;
}

const change = (current: string, next: string): string => JSON.stringify({ current, new: next });

/** `/vault/{namespace}/{database}/{vault}`, or the sentence that says which name is not one. */
function onePath(): { readonly path: string } | { readonly missing: string } {
  const names = [
    ["namespace", trimmed("vault-one-namespace")],
    ["database", trimmed("vault-one-database")],
    ["vault", trimmed("vault-one-name")],
  ] as const;
  const checked: string[] = [];
  for (const [what, name] of names) {
    if (name === "") return { missing: `name the ${what}` };
    const held = aName(name);
    if (held === null) return { missing: `${name} is not a ${what} name` };
    checked.push(held);
  }
  return { path: `/vault/${checked.join("/")}` };
}

function shape(): void {
  disable("vault-store-unseal", value("vault-store-passphrase") === "");
  disable("vault-store-change", value("vault-store-current") === "" || value("vault-store-new") === "");
  const one = onePath();
  write("vault-one-says", "path" in one ? `Acts on ${one.path}.` : one.missing);
  const named = !("path" in one);
  disable("vault-one-show", named);
  disable("vault-one-seal", named);
  disable("vault-one-unseal", named || value("vault-one-passphrase") === "");
  disable("vault-one-change", named || value("vault-one-current") === "" || value("vault-one-new") === "");
}

async function onOne(act_: (path: string) => Promise<void>): Promise<void> {
  const one = onePath();
  if ("path" in one) await act_(one.path);
  shape();
}

export function wire(): void {
  const store = (method: "GET" | "POST", path: string, body?: string): Promise<void> =>
    act(method, path, "vault-store-status", "vault-store-answer", body).finally(shape);
  const one = (method: "GET" | "POST", path: string, body?: string): Promise<void> =>
    act(method, path, "vault-one-status", "vault-one-answer", body);

  at("vault-store-refresh").addEventListener("click", () => void store("GET", "/vault"));
  at("vault-store-seal").addEventListener("click", () => void store("POST", "/vault/seal"));
  at("vault-store-unseal").addEventListener(
    "click",
    () => void store("POST", "/vault/unseal", spent("vault-store-passphrase")),
  );
  at("vault-store-change").addEventListener(
    "click",
    () => void store("POST", "/vault/passphrase", change(spent("vault-store-current"), spent("vault-store-new"))),
  );
  at("vault-one-show").addEventListener("click", () => void onOne((path) => one("GET", path)));
  at("vault-one-seal").addEventListener("click", () => void onOne((path) => one("POST", `${path}/seal`)));
  at("vault-one-unseal").addEventListener(
    "click",
    () => void onOne((path) => one("POST", `${path}/unseal`, spent("vault-one-passphrase"))),
  );
  at("vault-one-change").addEventListener(
    "click",
    () =>
      void onOne((path) =>
        one("POST", `${path}/passphrase`, change(spent("vault-one-current"), spent("vault-one-new"))),
      ),
  );
  for (const id of [
    "vault-store-passphrase",
    "vault-store-current",
    "vault-store-new",
    "vault-one-namespace",
    "vault-one-database",
    "vault-one-name",
    "vault-one-passphrase",
    "vault-one-current",
    "vault-one-new",
  ]) {
    at(id).addEventListener("input", shape);
  }
  shape();
  wireRecords();
}
