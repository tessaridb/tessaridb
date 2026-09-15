//! Talking to the node.
//!
//! The console is a client of the public API and has no private path to it.
//! Every request below is one a `curl` could make against the same node, which
//! is the constraint that keeps the API the product surface rather than
//! something this page sits on top of.

import { credential, ended, token } from "./session.js";

// The two public routes this page uses. Named once, so what the console reaches
// is greppable rather than spread through the file.
const SCRIPT_ROUTE = "/script";
export const WATCH_ROUTE = "/watch";

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

/** Run a script against the node, and hand back the reply and its text. */
export async function ask(source: string): Promise<{ reply: Response; text: string }> {
  const headers: Record<string, string> = {};
  const offered = credential();
  if (offered !== null) {
    headers["Authorization"] = offered;
  }
  const reply = await fetch(SCRIPT_ROUTE, {
    method: "POST",
    headers,
    body: source,
    // Without this the browser handles the node's `401` challenge itself and
    // opens its own credential dialog on top of the page — a second sign-in
    // this console did not ask for, cannot read and cannot clear, and which
    // leaves the page's own request hanging behind it. The credential is in the
    // header above; nothing here wants the browser to manage one.
    credentials: "omit",
  });
  if (reply.status === 401 && token() !== null) {
    ended();
  }
  return { reply, text: await reply.text() };
}

/** The single value a one-statement script answered, or a thrown reason. */
export async function valueOf(source: string): Promise<Result | null> {
  const { reply, text } = await ask(source);
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
