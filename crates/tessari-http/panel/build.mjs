// Renders the console's source into the committed assets.
//
// Nothing about building or running the database goes through here. The output
// under ../assets/ is committed and embedded by include_str!, so `cargo build`,
// `docker build` and a release all work on a machine with no Node at all — that
// is the property this build must never cost (ADR-0076 D3).
//
// The build is deterministic: run it twice and `git status` is clean the second
// time. That is the whole of the drift check, because there is no CI here and
// adding one is not ours to decide (Q-341).
//
// EVERY file under ../assets/ is written by this script, the stylesheet and the
// favicon included. They are copied rather than transformed, which buys nothing
// on its own — what it buys is that there is no longer a rule to remember about
// which files in that directory are safe to edit by hand. All of them are not.

import { build } from "esbuild";
import { copyFile, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { pathToFileURL } from "node:url";

const ASSETS = new URL("../assets/", import.meta.url);
const STATIC = new URL("./assets/", import.meta.url);

/** Bundle one TypeScript entry point and hand back the module it evaluates to. */
async function evaluated(entry) {
  const scratch = await mkdtemp(join(tmpdir(), "tessaridb-console-"));
  const bundled = join(scratch, "page.mjs");
  try {
    await build({
      entryPoints: [new URL(entry, import.meta.url).pathname],
      outfile: bundled,
      bundle: true,
      format: "esm",
      platform: "node",
      target: "node22",
      logLevel: "warning",
    });
    return await import(pathToFileURL(bundled).href);
  } finally {
    await rm(scratch, { recursive: true, force: true });
  }
}

/** Nothing the page loads may reach off this node. */
function local(what, text) {
  if (/\ssrc="https?:/i.test(text) || /\shref="https?:/i.test(text)) {
    // The panel ships inside a database image that is expected to run with no
    // network. An asset that reaches for a CDN turns a working console into a
    // blank page on exactly the machines this product is deployed on.
    throw new Error(`${what} reaches for something off this node`);
  }
}

// ------------------------------------------------------------------ the page

const page = await evaluated("./src/page.ts");
const html = page.index();
local("the emitted page", html);
await writeFile(new URL("index.html", ASSETS), html);
console.log(`index.html   ${html.length} bytes`);

// ---------------------------------------------------------------- the script

// One bundle, not two. `sections.js` used to read `console.js`'s top-level
// names out of the global scope; under a bundler the honest alternatives are
// real imports in one module graph, or two bundles that each inline their own
// copy of the session token — and the second one signs you in twice.
//
// Not minified. The asset is committed and embedded as text, and the tests that
// read it — and the person reading a diff — are checking the thing that ships.
await build({
  entryPoints: [new URL("./src/console.ts", import.meta.url).pathname],
  outfile: new URL("console.js", ASSETS).pathname,
  bundle: true,
  format: "iife",
  platform: "browser",
  target: "es2022",
  charset: "utf8",
  // `//!` file headers are "legal comments" to esbuild, and its default is to
  // sweep every one of them to the end of the bundle. Each module's header is
  // about the code directly under it, so a heap of them at the bottom is worse
  // than none at all.
  legalComments: "inline",
  logLevel: "warning",
});

const script = await readFile(new URL("console.js", ASSETS), "utf8");
local("the emitted script", script);

// Does it PARSE. The panel shipped dead in three releases because a duplicate
// top-level binding made the whole file a SyntaxError, and every check that
// existed — the ids resolve, the routes answer, the prose is true — passed the
// whole time, because none of them ever asked whether the code runs. `Function`
// compiles the source without executing it, which is exactly the question.
try {
  new Function(script);
} catch (failure) {
  throw new Error(`the emitted script does not parse: ${failure.message}`);
}
console.log(`console.js   ${script.length} bytes, parses`);

// ------------------------------------------------------- everything else

for (const name of ["console.css", "favicon.svg"]) {
  await copyFile(new URL(name, STATIC), new URL(name, ASSETS));
  console.log(`${name.padEnd(12)} copied`);
}
