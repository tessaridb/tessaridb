//! Whether this page shows the console or the sign-in page.
//!
//! The node decides, not this module. A store that has declared a user refuses
//! an unsigned script with `401`; one that has not answers it. So the gate asks
//! exactly that — one `RETURN 1;` through the console's one path to the node —
//! and draws what the answer means. Hiding the console is a courtesy: every
//! route behind it refuses an unsigned request on its own, and nothing here is
//! what keeps the data in.

import { ask, Unreachable } from "./api.js";
import { at, hide, say, write } from "./dom.js";
import { onIdentityChange, signedIn, token, told } from "./session.js";
import { hereAgain } from "./tabs.js";

/** Whether the store answered an unsigned script — true only while it has no users. */
let open = false;

export const storeIsOpen = (): boolean => open;

function showGate(words: string, failed: boolean): void {
  hide("shell", true);
  hide("gate", false);
  if (words !== "") {
    say("identity-status", words, failed);
  }
  at("user").focus();
}

function showConsole(): void {
  hide("gate", true);
  hide("shell", false);
  hide("open-banner", !open);
  const holding = token() !== null;
  hide("mine-open", !holding);
  hide("sign-out", !holding);
  if (open) {
    write("signed-in", "open store — no users");
  } else {
    signedIn();
  }
  hereAgain();
}

/** Ask the node whether it needs a sign-in, and show what the answer means. */
export async function decide(): Promise<void> {
  if (token() !== null) {
    open = false;
    showConsole();
    return;
  }
  try {
    const { reply } = await ask("RETURN 1;", "Sign-in · does this store need one");
    if (reply.status === 401) {
      open = false;
      showGate("", false);
      return;
    }
    if (reply.status >= 400) {
      // Something other than "sign in" — throttled, or broken. Saying it on the
      // sign-in page is the honest place, because the console would only repeat it.
      showGate(`the node answered ${reply.status} — try again in a moment`, true);
      return;
    }
    open = true;
    showConsole();
  } catch (failure) {
    showGate(
      failure instanceof Unreachable
        ? `the node did not answer: ${told(failure)}`
        : told(failure),
      true,
    );
  }
}

export function wire(): void {
  write("gate-where", window.location.host);
  // Enter in either field signs in: the button is the form's submit, and the
  // form itself never navigates anywhere.
  at("gate-form").addEventListener("submit", (event) => event.preventDefault());
  onIdentityChange(() => void decide());
  void decide();
}
