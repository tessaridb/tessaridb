//! Talking to the node.
//!
//! The console is a client of the public API and has no private path to it.
//! Every request below is one a `curl` could make against the same node, which
//! is the constraint that keeps the API the product surface rather than
//! something this page sits on top of.

import { record } from "./log.js";
import { credential, ended, token } from "./session.js";

// The two public routes this page uses. Named once, so what the console reaches
// is greppable rather than spread through the file.
const SCRIPT_ROUTE = "/script";
export const WATCH_ROUTE = "/watch";

/**
 * The request never reached the node.
 *
 * Its own type because the two failures need different sentences and a screen
 * cannot tell them apart from the message alone. `fetch` rejects with
 * `TypeError: Failed to fetch` when the node is stopped, and a screen that
 * prints that beside its own explanation of who may see a listing tells an
 * operator their PERMISSIONS are in question during an outage — which sends
 * them to check grants while the node is down. Measured in W320.
 */
export class Unreachable extends Error {}

/** One record as the node hands it over. */
export interface Row {
  readonly id: string;
  readonly value: unknown;
}

/**
 * One statement's answer.
 *
 * Every field is optional because this is a body the page did not construct:
 * it arrives off the wire and has to be checked before it is trusted, and a
 * type that promised `records` would only move that check somewhere quieter.
 */
export interface Result {
  readonly kind?: string;
  readonly records?: readonly Row[];
  readonly path?: string;
  readonly value?: unknown;
}

/** A whole script's answer, or the refusal that came instead. */
export interface Answer {
  readonly results?: readonly Result[];
  readonly error?: string;
}

/**
 * What one result amounts to, in the node's own vocabulary.
 *
 * It reports the `kind` the node used rather than a word this page chose for
 * it, and counts records rather than describing them. Anything more would be
 * the panel narrating an answer it did not give.
 */
const outcome = (result: Result): string => {
  if (Array.isArray(result.records)) {
    return result.records.length === 1 ? "1 record" : `${result.records.length} records`;
  }
  return result.kind ?? "answered";
};

/** The node's own words about what it just did — its answer, or its refusal. */
function said(text: string, status: number): { said: string; failed: boolean } {
  let body: Answer;
  try {
    body = JSON.parse(text) as Answer;
  } catch {
    // Not JSON at all: whatever it sent is still the node speaking, and an
    // empty body with a status is all there is to report.
    const words = text.trim();
    return { said: words === "" ? `${status}` : words, failed: status >= 400 };
  }
  if (typeof body.error === "string") {
    return { said: body.error, failed: true };
  }
  if (!Array.isArray(body.results)) {
    return { said: text, failed: status >= 400 };
  }
  return { said: body.results.map(outcome).join(", "), failed: status >= 400 };
}

/**
 * Run a script against the node, and hand back the reply and its text.
 *
 * `screen` is a parameter and not something read off the active tab, so that a
 * screen added later cannot forget to say who it is — the compiler asks. Every
 * call lands in the statement log, Run included: the log's job is that nothing
 * this session sent is unaccounted for, and the screen field is what tells the
 * operator's own statements apart from the ones a form sent for them.
 *
 * `why` is optional and carried rather than enforced: which actions owe a reason
 * is the screen's judgement, not this module's, and a transport that refused a
 * statement for want of a sentence would be deciding policy from the wrong
 * place. The screens that owe one refuse before they reach here.
 */
export async function ask(
  source: string,
  screen: string,
  why?: string,
): Promise<{ reply: Response; text: string }> {
  const started = performance.now();
  const headers: Record<string, string> = {};
  const offered = credential();
  if (offered !== null) {
    headers["Authorization"] = offered;
  }
  let reply: Response;
  try {
    reply = await fetch(SCRIPT_ROUTE, {
      method: "POST",
      headers,
      body: source,
      // Without this the browser handles the node's `401` challenge itself and
      // opens its own credential dialog on top of the page — a second sign-in
      // this console did not ask for, cannot read and cannot clear, and which
      // leaves the page's own request hanging behind it. The credential is in
      // the header above; nothing here wants the browser to manage one.
      credentials: "omit",
    });
  } catch {
    // Whatever the browser called it, what happened is that nothing answered.
    throw new Unreachable("the node did not answer — it may be stopped or unreachable");
  }
  if (reply.status === 401 && token() !== null) {
    ended();
  }
  const text = await reply.text();
  record({
    what: source,
    ...said(text, reply.status),
    ms: Math.round(performance.now() - started),
    screen,
    why,
  });
  return { reply, text };
}

/** The single value a one-statement script answered, or a thrown reason. */
export async function valueOf(
  source: string,
  screen: string,
  why?: string,
): Promise<Result | null> {
  const { reply, text } = await ask(source, screen, why);
  let body: Answer;
  try {
    body = JSON.parse(text) as Answer;
  } catch {
    throw new Error(text.trim() === "" ? reply.status + " " + reply.statusText : text);
  }
  if (!Array.isArray(body.results)) {
    // A refusal is JSON too, and its message is the useful half.
    throw new Error(typeof body.error === "string" ? body.error : text);
  }
  const answered = body.results[body.results.length - 1];
  return answered === undefined ? null : answered;
}

/** The object a `value` result carries, or `null` when it carries none. */
export function held(answered: Result | null): Record<string, unknown> | null {
  if (
    answered === null ||
    answered.kind !== "value" ||
    typeof answered.value !== "object" ||
    answered.value === null
  ) {
    return null;
  }
  return answered.value as Record<string, unknown>;
}

/** One operational route, parsed as JSON, or a reason it could not be. */
export async function scrape(route: string): Promise<{ status: number; body: unknown }> {
  // Same reason as `ask`: no browser-managed credential, no native dialog.
  const reply = await fetch(route, { credentials: "omit" });
  const text = await reply.text();
  try {
    return { status: reply.status, body: JSON.parse(text) as unknown };
  } catch {
    return { status: reply.status, body: text };
  }
}
