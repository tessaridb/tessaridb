//! Changing your own password.

import { at, disable, say, setValue, trimmed, value } from "./dom.js";
import { record } from "./log.js";
import { basic, reason, told } from "./session.js";

/**
 * Keep the button honest about whether the three fields agree.
 *
 * The new one twice, because this is the one field on the page whose value
 * nobody can read back: a typo here is discovered at the next sign-in, by
 * somebody who no longer knows what they typed.
 */
export function shapeMine(): void {
  const current = value("mine-current");
  const fresh = value("mine-new");
  const again = value("mine-again");
  disable("mine", current === "" || fresh === "" || fresh !== again);
  const differ = fresh !== "" && again !== "" && fresh !== again;
  say("mine-status", differ ? "the two new ones differ" : "", differ);
}

export function wire(): void {
  for (const field of ["mine-current", "mine-new", "mine-again"]) {
    at(field).addEventListener("input", shapeMine);
  }

  at("mine").addEventListener("click", async () => {
    const name = trimmed("user");
    if (name === "") {
      say("mine-status", "sign in first — this changes your own password", true);
      return;
    }
    say("mine-status", "changing…");
    const started = performance.now();
    try {
      // Basic and not the token this page is holding: the route asks for the
      // current password as a second proof, and a token is not one. The bytes
      // are encoded the same way `credential()` does it, for the same reason.
      const reply = await fetch("/password", {
        method: "POST",
        headers: { Authorization: "Basic " + basic(name, value("mine-current")) },
        body: value("mine-new"),
        credentials: "omit",
      });
      const text = await reply.text();
      // This is the one thing the panel does that is not a statement, so the
      // log records the ACT rather than a sentence of TessariQL. Leaving it out
      // would make the log's claim false in exactly the place it matters most:
      // this ends every session the account holds, on every node.
      record({
        what: "Change your own password — POST /password as " + name,
        said: reply.status >= 400 ? reason(text) : "changed; every session ended",
        failed: reply.status >= 400,
        ms: Math.round(performance.now() - started),
        screen: "Users · your own password",
      });
      if (reply.status >= 400) {
        say("mine-status", reason(text), true);
        return;
      }
      // Every token is dead now, this page's included, so it is signed out here
      // rather than left to fail on the next button somebody presses.
      for (const field of ["mine-current", "mine-new", "mine-again"]) {
        setValue(field, "");
      }
      shapeMine();
      at("sign-out").click();
      say("identity-status", "password changed — sign in with the new one", false);
    } catch (failure) {
      say("mine-status", "the node did not answer: " + told(failure), true);
    }
  });

  shapeMine();
}
