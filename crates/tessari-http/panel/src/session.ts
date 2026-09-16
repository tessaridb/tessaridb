//! Who this page is, and the token it is holding.
//!
//! The token lives in this module's scope. That is the whole reason the panel
//! is ONE bundle: two bundles would each inline a copy of this module, so there
//! would be two tokens, and signing in on one section would silently not sign
//! in the other.

import { at, say, setValue, value, write } from "./dom.js";
import { hereAgain } from "./tabs.js";

/**
 * The token this page is holding, or `null`.
 *
 * A password is spent once at `POST /session` and this is what comes back. It
 * lives in a variable and not in `localStorage` deliberately: a bearer token in
 * storage outlives the tab, survives the reader walking away, and is readable by
 * anything that ever manages to run script on this origin. Closing the tab
 * should end the session, and here it does.
 */
let held: string | null = null;

/** The token, for the two callers that need it as a value. */
export const token = (): string | null => held;

/** A `Basic` value for what is in the two fields, or nothing when both are empty. */
export function typed(): string | null {
  const user = value("user");
  const password = value("password");
  if (user === "" && password === "") {
    return null;
  }
  // `btoa` throws on any character above U+00FF, so a password with an accent
  // in it would break the button rather than be refused by the node — and the
  // node decodes the header as UTF-8, so it would have accepted one. Encode
  // first, then base64 the bytes.
  return "Basic " + basic(user, password);
}

/** A name and a password as the base64 of their UTF-8 bytes. */
export function basic(user: string, password: string): string {
  const bytes = new TextEncoder().encode(user + ":" + password);
  return btoa(String.fromCharCode(...bytes));
}

/** The `Authorization` value to send, or nothing when there is nothing to send. */
export function credential(): string | null {
  // The token wins whenever there is one. It costs the node a hash-map lookup
  // where a password costs nineteen mebibytes of Argon2, which is the whole
  // reason `POST /session` exists.
  if (held !== null) {
    return "Bearer " + held;
  }
  return typed();
}

/** Keep the collapsed identity control honest about whether there is a session. */
export function signedIn(): void {
  const user = value("user");
  if (held !== null && user !== "") {
    write("signed-in", user);
    return;
  }
  // A name typed but not yet exchanged for a token is not a session, and saying
  // so is the difference between "this will work" and "this might".
  write("signed-in", user === "" ? "not signed in" : user + " — not yet");
}

/**
 * The token stopped working, so stop holding it.
 *
 * A token that stopped working stopped for a reason worth acting on: somebody
 * rotated the password, changed the role, or removed the account. Holding on to
 * it would make every request afterwards fail with the same refusal and none of
 * them say why.
 */
export function ended(): void {
  held = null;
  signedIn();
  say("identity-status", "this session ended — sign in again", true);
  // The screen in front of the reader is still showing what the dead session
  // was told. It reads again and is refused, which is the true answer; leaving
  // it is the console presenting one identity's data under another's name.
  //
  // It cannot loop: `ask` only calls this while a token is held, and the line
  // above dropped it.
  hereAgain();
}

/** The `error` out of a refusal's body, or the body when it is not one. */
export function reason(text: string): string {
  try {
    const body: unknown = JSON.parse(text);
    if (typeof body === "object" && body !== null && "error" in body) {
      const held = (body as { error: unknown }).error;
      if (typeof held === "string") {
        return held;
      }
    }
    return text;
  } catch {
    // Not JSON, so the text is already the most useful thing there is.
    return text;
  }
}

/** The disclosure this control lives in, closed by everything that should. */
function sheet(): HTMLDetailsElement {
  const found = document.querySelector("details.identity");
  if (!(found instanceof HTMLDetailsElement)) {
    throw new Error("the page has no identity disclosure");
  }
  return found;
}

export function wire(): void {
  const identity = sheet();

  at("user").addEventListener("input", signedIn);

  // Spend the password once, keep the token, and close the sheet.
  //
  // This is a real sign-in and not a check any more: the node verifies the
  // password here and hands back a token, and every request after this one
  // carries the token instead. The password is then cleared from the field,
  // because there is nothing left that needs it and a credential sitting in a
  // DOM input is a credential in every screenshot, screen share and browser
  // extension that reads the page.
  at("sign-in").addEventListener("click", async () => {
    const offered = typed();
    if (offered === null) {
      say("identity-status", "a name and a password, or nothing at all", true);
      return;
    }
    say("identity-status", "signing in…");
    try {
      const reply = await fetch("/session", {
        method: "POST",
        headers: { Authorization: offered },
        // The same reason `ask` omits them: left to itself the browser answers
        // the node's `401` challenge with its own credential dialog, which this
        // console did not ask for and cannot clear.
        credentials: "omit",
      });
      const text = await reply.text();
      if (reply.status >= 400) {
        say("identity-status", reason(text), true);
        return;
      }
      held = (JSON.parse(text) as { token: string }).token;
      setValue("password", "");
      say("identity-status", "");
      signedIn();
      identity.open = false;
      // The screen behind this sheet asked the node before there was anything
      // to ask with, and was told so. Signing in is the answer to the message
      // it is still displaying, so it reads again now rather than when the
      // reader happens to navigate — W320 measured an operator signing in and
      // being left looking at "this store requires a signed-in user".
      hereAgain();
    } catch (failure) {
      say("identity-status", "the node did not answer: " + told(failure), true);
    }
  });

  // Hand the token back, then forget it here.
  //
  // Told to the node rather than only dropped locally: a token this page
  // forgets without saying so stays live on the node until it expires, which is
  // the difference between signing out and closing your eyes.
  at("sign-out").addEventListener("click", async () => {
    if (held !== null) {
      try {
        await fetch("/session", {
          method: "DELETE",
          headers: { Authorization: "Bearer " + held },
          credentials: "omit",
        });
      } catch {
        // The node is unreachable. Forgetting it here is still right — the token
        // expires on its own, and staying signed in because the network failed
        // is the wrong way to be wrong.
      }
    }
    held = null;
    setValue("user", "");
    setValue("password", "");
    say("identity-status", "");
    signedIn();
    identity.open = false;
    // Signing out has to reach the screen as well as the node. What is drawn
    // is the previous identity's answer, and leaving it there is the same
    // disclosure as never having signed out at all.
    hereAgain();
  });

  // A sheet that hangs over the page until something else is clicked is the
  // complaint this control earned. Escape and a click outside both close it,
  // which is what every other disclosure on the web does.
  identity.addEventListener("keydown", (event) => {
    if (event.key === "Escape") {
      identity.open = false;
    }
  });

  document.addEventListener("click", (event) => {
    const target = event.target;
    if (identity.open && target instanceof globalThis.Node && !identity.contains(target)) {
      identity.open = false;
    }
  });

  signedIn();
}

/** What a thrown thing said, for a status line that has to say something. */
export function told(failure: unknown): string {
  return failure instanceof Error ? failure.message : String(failure);
}
