// Users, node and cluster.
//
// Everything here goes through the same two things the query section uses: a
// statement over `POST /script`, or one of the operational routes any monitor
// already scrapes. There is no request on this page that a `curl` could not
// make, which is what keeps a console feature from becoming a capability only
// the console has.
//
// `console.js` is loaded first and its top-level names — `at`, `say`, `ask`,
// `drawn` — are in scope here. Two files rather than one because they are two
// concerns, not because either is too long.

"use strict";

const HEALTH_ROUTE = "/health";
const READY_ROUTE = "/ready";
const METRICS_ROUTE = "/metrics";

/** Text into a TessariQL string literal, escaped the way the language escapes. */
function quoted(text) {
  let out = "'";
  for (const character of text) {
    if (character === "'") {
      out += "\\'";
    } else if (character === "\\") {
      out += "\\\\";
    } else if (character === "\n") {
      out += "\\n";
    } else if (character === "\r") {
      out += "\\r";
    } else if (character === "\t") {
      out += "\\t";
    } else {
      out += character;
    }
  }
  return out + "'";
}

/** Draw a value into a pane: a table where it is one, JSON where it is not. */
function put(where, value) {
  const pane = at(where);
  pane.textContent = "";
  const shown = document.createElement("pre");
  shown.textContent = JSON.stringify(value, null, 2);
  pane.appendChild(shown);
}

/** Draw a flat object as a two-column table of name and value. */
function facts(where, held) {
  const pane = at(where);
  pane.textContent = "";
  const table = document.createElement("table");
  const body = table.createTBody();
  for (const [name, value] of Object.entries(held)) {
    const row = body.insertRow();
    const label = document.createElement("th");
    label.textContent = name;
    row.appendChild(label);
    const box = row.insertCell();
    // `textContent`: these values come out of the store and out of a config
    // file, and either could hold something that looks like markup.
    box.textContent = typeof value === "string" ? value : JSON.stringify(value);
    if (typeof value === "number") {
      box.classList.add("number");
    }
  }
  pane.appendChild(table);
}

/** The single value a one-statement script answered, or a thrown reason. */
async function valueOf(source) {
  const { reply, text } = await ask(source);
  let body = null;
  try {
    body = JSON.parse(text);
  } catch (ignored) {
    throw new Error(text.trim() === "" ? reply.status + " " + reply.statusText : text);
  }
  if (!Array.isArray(body.results)) {
    // A refusal is JSON too, and its message is the useful half.
    throw new Error(typeof body.error === "string" ? body.error : text);
  }
  const answered = body.results[body.results.length - 1];
  return answered === undefined ? null : answered;
}

// -------------------------------------------------------------------- users

at("lookup").addEventListener("click", async () => {
  const name = at("lookup-name").value.trim();
  if (name === "") {
    say("user-status", "a name is needed — there is no listing to pick from", true);
    return;
  }
  say("user-status", "asking…");
  try {
    const answered = await valueOf("INFO FOR USER " + name + ";");
    if (answered !== null && answered.kind === "value" && answered.value !== undefined) {
      facts("user-answer", answered.value);
    } else {
      put("user-answer", answered);
    }
    say("user-status", "");
  } catch (failure) {
    at("user-answer").textContent = "";
    say("user-status", failure.message, true);
  }
});

/** The statement the form describes, shown before it is run and never after. */
function definition() {
  const name = at("new-name").value.trim();
  const scope = at("new-scope").value.trim();
  const role = at("new-role").value.trim();
  if (name === "" || role === "") {
    return null;
  }
  return (
    "DEFINE USER " +
    name +
    (scope === "" ? "" : " ON " + scope) +
    " ROLE " +
    role +
    " PASSWORD " +
    quoted(at("new-password").value) +
    ";"
  );
}

/** Keep the preview current, with the password shown as the store shows it. */
function preview() {
  const statement = definition();
  at("define-preview").textContent =
    statement === null
      ? "a name and a role are needed"
      : // The preview is the one place the password would appear in plain view
        // on somebody's screen, and a shoulder is a threat this page can
        // actually do something about. The statement that runs carries the real
        // one; this is a drawing of it.
        statement.replace(/PASSWORD '.*';$/, "PASSWORD '…';");
}

for (const field of ["new-name", "new-scope", "new-role", "new-password"]) {
  at(field).addEventListener("input", preview);
}

at("define").addEventListener("click", async () => {
  const statement = definition();
  if (statement === null) {
    say("define-status", "a name and a role are needed", true);
    return;
  }
  say("define-status", "running…");
  try {
    const answered = await valueOf(statement);
    say("define-status", answered !== null && answered.kind === "done" ? "ok" : "");
    if (answered === null || answered.kind !== "done") {
      put("user-answer", answered);
    }
  } catch (failure) {
    say("define-status", failure.message, true);
  }
});

// --------------------------------------------------------------------- node

/** One operational route, parsed as JSON, or a reason it could not be. */
async function scrape(route) {
  const reply = await fetch(route);
  const text = await reply.text();
  try {
    return { status: reply.status, body: JSON.parse(text) };
  } catch (ignored) {
    return { status: reply.status, body: text };
  }
}

/**
 * Prometheus text as the pairs it carries.
 *
 * Comment lines are the type and help, which a person reading a console does
 * not need beside the number. A line's value is what follows its last space,
 * because the name may carry labels and a label may carry a space.
 */
function readings(text) {
  const out = {};
  for (const line of text.split("\n")) {
    if (line.startsWith("#") || line.trim() === "") {
      continue;
    }
    const cut = line.lastIndexOf(" ");
    if (cut > 0) {
      out[line.slice(0, cut)] = Number(line.slice(cut + 1));
    }
  }
  return out;
}

async function readNode() {
  say("node-status", "asking…");
  try {
    const answered = await valueOf("INFO FOR NODE;");
    const held =
      answered !== null && answered.kind === "value" && answered.value !== undefined
        ? answered.value
        : {};
    // `cluster` is its own object and belongs on the cluster tab; what is left
    // is this machine, which is what this pane claims to show.
    const { cluster, ...mine } = held;
    facts("node-facts", mine);
    facts("cluster-facts", {
      membership: held.membership,
      peers: cluster === undefined ? [] : cluster.peers,
      endpoints: held.endpoints,
      id: held.id,
    });
    say("cluster-status", "");
  } catch (failure) {
    at("node-facts").textContent = "";
    say("node-status", failure.message, true);
    say("cluster-status", failure.message, true);
    return;
  }

  const [health, ready, metrics] = await Promise.all([
    scrape(HEALTH_ROUTE),
    scrape(READY_ROUTE),
    scrape(METRICS_ROUTE),
  ]);
  // The status and the body say the same thing twice when both are well, and
  // different things when they are not — which is the case worth reading, so
  // each one is drawn as the status followed by what the node said.
  const answer = (scraped) =>
    scraped.status +
    " " +
    (typeof scraped.body === "object" && scraped.body !== null
      ? Object.entries(scraped.body)
          .map(([name, value]) => name + " " + value)
          .join(", ")
      : String(scraped.body).trim());
  facts("node-health", { health: answer(health), ready: answer(ready) });
  facts(
    "node-metrics",
    typeof metrics.body === "string" ? readings(metrics.body) : metrics.body,
  );
  say("node-status", "");
}

at("node-refresh").addEventListener("click", readNode);

// Read once when the section is first opened, rather than on load: a console
// left on the query tab should not be scraping a node nobody is looking at.
let nodeRead = false;
for (const tab of ["tab-node", "tab-cluster"]) {
  at(tab).addEventListener("click", () => {
    if (!nodeRead) {
      nodeRead = true;
      readNode();
    }
  });
}

preview();
