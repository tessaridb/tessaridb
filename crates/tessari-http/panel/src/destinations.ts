//! Where the console can take you.
//!
//! Its own module because both `page.ts` and every screen need it, and a screen
//! importing it back out of the page that assembles the screens is a cycle.

import { type Destination } from "./ui.js";

/**
 * The four destinations, named for the JOB rather than for the noun.
 *
 * `Query`, `Users`, `Node` and `Cluster` named the things the console holds.
 * These name what somebody came to do: run a statement, look after the cluster,
 * decide who may reach what, see what this process is. The difference shows the
 * moment an operator arrives knowing their task and not the product's vocabulary.
 *
 * The cap is FOUR. A fifth destination is a trade to be recorded and argued for,
 * not a place to put the next screen — S3's whole claim is that navigation stays
 * readable at a glance, and a glance does not scale.
 */
export const DESTINATIONS: readonly Destination[] = [
  { name: "run", label: "Run" },
  { name: "cluster", label: "Cluster" },
  { name: "access", label: "Access" },
  { name: "this-node", label: "This node" },
];

/**
 * The destination with this name.
 *
 * By name rather than by position, because the panes used to index into the list
 * above and a reorder would have moved every pane to the wrong tab **silently**:
 * the ids still resolve, the counts still match, and the only symptom is that
 * `Cluster` shows the user forms. Looking a destination up by what it is called
 * makes that failure inexpressible.
 */
export const to = (name: string): Destination => {
  const found = DESTINATIONS.find((destination) => destination.name === name);
  if (found === undefined) {
    throw new Error("no destination named " + name);
  }
  return found;
};
