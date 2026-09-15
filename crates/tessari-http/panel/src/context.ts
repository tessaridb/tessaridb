//! What you had typed, still there after the reload.
//!
//! An operator mid-incident loses a session to a refresh, a crashed tab, a
//! laptop lid. What they lose with it is a script they had built up, a name they
//! were looking at, a form half filled in — every one of which they will now
//! reconstruct from memory, in the worst conditions for reconstructing anything.
//!
//! # Not a password, ever
//!
//! Nothing whose control is `type="password"` is written here, and that is
//! enforced by reading the control rather than by keeping the list correct: a
//! field added to the list later cannot become a stored credential by somebody
//! forgetting which kind it was. The console's whole posture is that it holds a
//! token in memory and nothing on disk, and a remembered password would quietly
//! be the exception to that on a machine two people share.
//!
//! # `sessionStorage`, not `localStorage`
//!
//! Per tab, gone when the tab closes, never shared with another tab. A reload is
//! the interruption this exists for; a colleague opening the console tomorrow on
//! the same machine is not, and `localStorage` would hand them the namespace, the
//! account name and the statement somebody was working on last night.
//!
//! # What does NOT survive, said plainly
//!
//! A running follow does not resume. The token that authorised it lives in
//! memory and the reload took it, so re-establishing the socket would need a
//! sign-in the operator has not given yet. The fields come back and the screen
//! says the follow stopped — which is the honest half of the claim, where
//! silently not resuming would leave somebody watching a feed that is not there.

import { at } from "./dom.js";
import { isFollowing } from "./watch.js";

const KEY = "tessaridb.console.context";

/**
 * Whether a follow was running when the page went away.
 *
 * Not a field, so it is kept beside them under a name no field can have. It
 * replaces `kept["namespace"] !== undefined`, which was a test of whether the
 * Namespace box had text in it — and that box is PREFILLED by the served page.
 * So every restore announced *the follow stopped at the reload* to operators
 * who had never pressed Follow, which is a small lie told reliably.
 */
const FOLLOWING = "#following";

/**
 * Every field whose content is the operator's own work.
 *
 * Statements, names, and the shape of a follow. Not a role choice or a tick —
 * those are one click to set and carry no typing, and remembering them would
 * mean a form that reopens pre-armed in a state nobody chose this session.
 */
const REMEMBERED: readonly string[] = [
  "script",
  "lookup-name",
  "user-filter",
  "new-name",
  "new-scope",
  "change-name",
  "remove-name",
  "remove-why",
  "change-why",
  "namespace",
  "database",
  "table",
  "from",
  "search",
];

/** Whether this control may be written down at all. */
function mayKeep(id: string): boolean {
  const control = at(id);
  return !(control instanceof HTMLInputElement && control.type === "password");
}

function held(): Record<string, string> {
  try {
    const found = window.sessionStorage.getItem(KEY);
    return found === null ? {} : (JSON.parse(found) as Record<string, string>);
  } catch {
    // Private windows and storage-disabled browsers throw on access rather than
    // answering empty. A console that would not load because it could not
    // remember a draft would have made the smaller feature the bigger risk.
    return {};
  }
}

function keep(): void {
  const kept: Record<string, string> = {};
  for (const id of REMEMBERED) {
    if (!mayKeep(id)) {
      continue;
    }
    const value = (at(id) as HTMLInputElement | HTMLTextAreaElement).value;
    if (value !== "") {
      kept[id] = value;
    }
  }
  if (isFollowing()) {
    kept[FOLLOWING] = "yes";
  }
  try {
    window.sessionStorage.setItem(KEY, JSON.stringify(kept));
  } catch {
    // Full, or refused. Losing a draft is the cost; failing to type is not.
  }
}

/** Put back what was there, and say what could not come back with it. */
function restore(): void {
  const kept = held();
  for (const id of REMEMBERED) {
    const value = kept[id];
    if (value === undefined || !mayKeep(id)) {
      continue;
    }
    (at(id) as HTMLInputElement | HTMLTextAreaElement).value = value;
  }
  if (kept[FOLLOWING] === "yes") {
    // The one thing that does not come back, said where it was happening — and
    // said only to somebody who actually lost one.
    at("watch-status").textContent =
      "the follow stopped at the reload — sign in and press Follow to resume it";
  }
}

export function wire(): void {
  restore();
  for (const id of REMEMBERED) {
    at(id).addEventListener("input", keep);
  }
  // Also on the way out, so a close-and-reopen keeps the last keystroke rather
  // than the last one that happened to fire an input event.
  window.addEventListener("beforeunload", keep);
}
