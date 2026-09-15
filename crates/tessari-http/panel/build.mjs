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

import { build } from "esbuild";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { pathToFileURL } from "node:url";

const ASSETS = new URL("../assets/", import.meta.url);

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

const page = await evaluated("./src/page.ts");
const html = page.index();

if (/\ssrc="https?:/i.test(html) || /\shref="https?:/i.test(html)) {
  // The panel ships inside a database image that is expected to run with no
  // network. An asset that reaches for a CDN turns a working console into a
  // blank page on exactly the machines this product is deployed on.
  throw new Error("the emitted page reaches for something off this node");
}

await writeFile(new URL("index.html", ASSETS), html);
console.log(`index.html  ${html.length} bytes`);
