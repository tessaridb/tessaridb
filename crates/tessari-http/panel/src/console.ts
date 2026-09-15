//! The console's entry point: everything the page does, started in one place.
//!
//! Nothing here is fetched from anywhere else: no framework, no CDN, no web
//! font. The page is meant to work on a machine with no route out at all, and a
//! single remote reference would quietly take that away.
//!
//! This is ONE bundle on purpose. `sections.js` used to be a second file that
//! read the first one's top-level names out of the global scope, and that
//! coupling is what turned a single `SyntaxError` in one file into a dead page
//! — every section of it, for nineteen days. Two bundles would be no better:
//! each would inline its own copy of `session.ts`, so there would be two tokens
//! and signing in on one section would silently not sign in the other.
//!
//! The start-up order is written out below rather than left to emerge from the
//! import graph. In a bundle, a module's top-level statements run when the
//! graph first reaches it, which makes the order of everything on this page an
//! accident of who imports whom. One list is cheaper to read and cannot drift.

import { write } from "./dom.js";
import * as context from "./context.js";
import * as detail from "./detail.js";
import * as drawer from "./drawer.js";
import * as formation from "./formation.js";
import * as log from "./log.js";
import * as node from "./node.js";
import * as password from "./password.js";
import * as query from "./query.js";
import * as search from "./search.js";
import * as session from "./session.js";
import * as shortcuts from "./shortcuts.js";
import * as tabs from "./tabs.js";
import * as userForms from "./user-forms.js";
import * as users from "./users.js";
import * as watch from "./watch.js";

write("where", "served by " + window.location.host);

// The log first, so the count in the tray reads zero before anything can add
// to it — and so a statement sent during start-up has somewhere to land.
log.wire();
tabs.wire();
session.wire();
query.wire();
search.wire();
shortcuts.wire();
formation.wire();
drawer.wire();
detail.wire();

// Last: a restored value fires the same handlers typing would, and those
// handlers belong to modules that have to be wired before it does.
context.wire();
watch.wire();
userForms.wire();
users.wire();
node.wire();
password.wire();
