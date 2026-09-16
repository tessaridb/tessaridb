//! What the panel has been told about who exists.
//!
//! The blast radius a destructive form shows — *who loses what* — has to come
//! from somewhere, and the only honest source is the listing the node already
//! answered. This holds it, so that the forms can read it without importing the
//! listing that draws it: `users.ts` writes here and `user-forms.ts` reads, which
//! keeps the two modules pointing one way instead of at each other.
//!
//! It is deliberately not a cache. Nothing here is asked for on a miss, nothing
//! expires, and a name this module has never heard of returns `null` so the form
//! can say it does not know rather than invent a reach. A radius drawn from a
//! guess is worse than no radius at all — it is the panel narrating an answer
//! the node never gave.

/** What a listing said about one account. */
export interface Known {
  /** The role as the node named it. */
  readonly role: string;
  /** The tenancy in the words the listing used — `the whole node` included. */
  readonly reach: string;
}

const known = new Map<string, Known>();

/** Start again, because a fresh listing is the whole truth about who exists. */
export function forget(): void {
  known.clear();
}

export function remember(name: string, one: Known): void {
  known.set(name, one);
}

/** What the last listing said about `name`, or `null` if it never said. */
export function lookup(name: string): Known | null {
  return known.get(name) ?? null;
}
