//! The four things a screen can be, said as four things.
//!
//! `say(id, words, failed?)` carries one string and one boolean, so the five
//! situations an operator actually meets render as two. The two that collapse
//! are the expensive ones: **nothing here** and **some of it** look identical,
//! and an operator who reads a bounded page as the whole list concludes an
//! account does not exist when it is simply not on screen.
//!
//! # Each state names a NEXT ACTION, and that is the half worth guarding
//!
//! "No users" is a state. "No users — this store is open to anybody, and the
//! first `DEFINE USER` closes it" is a state that tells the reader what to do
//! about it. A screen full of correct nouns and no verbs is a screen that makes
//! the operator go and ask somebody.
//!
//! # Why not simply widen `say`
//!
//! Because a wider `say` would let a screen render a partial state by passing
//! the wrong argument, and nothing would say so. Four named calls make the wrong
//! one a thing you have to type on purpose. `say` keeps its two-state job for
//! the many places that genuinely have two — this is not a rewrite of all
//! sixty-six of its call sites, and it must not become one.

import { at } from "./dom.js";

/** What a screen is, right now. */
export type Kind =
  /** A request is out and nothing has come back. */
  | "waiting"
  /** The node answered, and the answer holds nothing. */
  | "empty"
  /** The node answered, and what is on screen is less than what it said. */
  | "partial"
  /** The node refused, or the request never arrived. */
  | "wrong";

/**
 * Put one of the four states on screen.
 *
 * The class is the state's own, so the four are distinguishable to a stylesheet
 * and to a test without either reading the words. The words are still the
 * screen's job: what "empty" means on the Access list and on the cluster map
 * are different sentences, and a shared one would be vague enough to fit both.
 */
export function state(id: string, kind: Kind, words: string): void {
  const line = at(id);
  line.textContent = words;
  for (const other of ["waiting", "empty", "partial", "wrong"]) {
    line.classList.toggle(`is-${other}`, other === kind);
  }
  // `failed` is what the stylesheet and the older call sites already key on for
  // the red treatment. Kept in step rather than replaced, so a screen that mixes
  // `say` and `state` does not mix two appearances of the same idea.
  line.classList.toggle("failed", kind === "wrong");
  // A refusal is the one of the four an operator must not miss while reading
  // something else, so it is announced rather than merely rendered.
  line.setAttribute("aria-live", kind === "wrong" ? "assertive" : "polite");
}

/** Clear the state entirely — the answer arrived and it speaks for itself. */
export function settled(id: string): void {
  const line = at(id);
  line.textContent = "";
  for (const other of ["waiting", "empty", "partial", "wrong"]) {
    line.classList.remove(`is-${other}`);
  }
  line.classList.remove("failed");
}
