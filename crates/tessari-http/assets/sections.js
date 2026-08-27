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

/** Everybody the caller is allowed to be told about. */
async function listUsers() {
  say("user-status", "asking…");
  try {
    const answered = await valueOf("INFO FOR USERS;");
    const held =
      answered !== null && answered.kind === "value" && answered.value !== undefined
        ? answered.value.users
        : [];
    const pane = at("user-list");
    pane.textContent = "";
    if (!Array.isArray(held) || held.length === 0) {
      const empty = document.createElement("p");
      empty.className = "trailer";
      empty.textContent = "(no users — this store is open to anybody)";
      pane.appendChild(empty);
      say("user-status", "");
      return;
    }
    const table = document.createElement("table");
    const head = table.createTHead().insertRow();
    for (const column of ["user", "role", "reach"]) {
      const cell = document.createElement("th");
      cell.textContent = column;
      head.appendChild(cell);
    }
    const body = table.createTBody();
    for (const one of held) {
      const row = body.insertRow();
      row.insertCell().textContent = one.user;
      // An owner with no space is the node's administrator. The listing says so
      // in the word people use, rather than leaving it to be inferred from an
      // empty cell — which is what the panel did before, and nobody inferred it.
      row.insertCell().textContent =
        one.role === "owner" && one.namespace === undefined ? "owner · admin" : one.role;
      row.insertCell().textContent =
        one.namespace === undefined
          ? "the whole node"
          : one.namespace + (one.database === undefined ? "" : "." + one.database);
      // A name in a listing is there to be clicked; typing it again is the kind
      // of small tax that makes an operator go back to `curl`.
      row.addEventListener("click", () => {
        at("lookup-name").value = one.user;
        // Both forms, because a name picked out of a listing is picked in order
        // to do something to it, and which of the two comes next is not
        // knowable from the click.
        at("change-name").value = one.user;
        at("remove-name").value = one.user;
        shapeTheChange();
        // Deliberately NOT the confirmation field: a click that filled
        // both would arm the destructive button by itself.
        shapeTheRemoval();
        at("lookup").click();
      });
    }
    pane.appendChild(table);
    say("user-status", "");
  } catch (failure) {
    at("user-list").textContent = "";
    say("user-status", failure.message, true);
  }
}

at("list").addEventListener("click", listUsers);

at("tab-users").addEventListener("click", () => {
  if (at("user-list").textContent === "") {
    listUsers();
  }
});

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

/** What each role means, said beside the control rather than inside it. */
const MEANS = {
  viewer: "reads what the space holds, and nothing else.",
  editor: "reads and writes records, and declares structure.",
  owner: "everything in the space, users included.",
  other:
    "a role this build may not know. It will be sent as typed, and the node's" +
    " refusal is what you will see if it does not exist.",
};

/**
 * The tenancy the form describes: a space, or none at all.
 *
 * `null` means the form is asking for a space and has not been given one. It is
 * distinct from `""`, which means the whole node — and conflating the two is how
 * an empty field silently produces an administrator of everything. That is the
 * exact failure this form was split in two to prevent, and it happened here
 * before this returned three answers instead of two.
 */
function reach() {
  if (at("new-reach").value === "node") {
    return "";
  }
  const space = at("new-scope").value.trim();
  return space === "" ? null : space;
}

/** The role the form describes, which may be one this build has never heard of. */
function role() {
  const chosen = at("new-role").value;
  return chosen === "other" ? at("new-role-other").value.trim() : chosen;
}

/** The statement the form describes, shown before it is run and never after. */
function definition() {
  const name = at("new-name").value.trim();
  const named = role();
  const space = reach();
  if (name === "" || named === "" || space === null) {
    return null;
  }
  return (
    "DEFINE USER " +
    name +
    (space === "" ? "" : " ON " + space) +
    " ROLE " +
    named +
    " PASSWORD " +
    quoted(at("new-password").value) +
    ";"
  );
}

/** Keep the preview current, with the password shown as the store shows it. */
function preview() {
  const statement = definition();
  at("role-says").textContent = MEANS[at("new-role").value] ?? "";
  at("define-preview").textContent =
    statement === null
      ? missing()
      : // The preview is the one place the password would appear in plain view
        // on somebody's screen, and a shoulder is a threat this page can
        // actually do something about. The statement that runs carries the real
        // one; this is a drawing of it.
        statement.replace(/PASSWORD '.*';$/, "PASSWORD '…';");
}

/** Which field is still empty, named rather than left to be guessed. */
function missing() {
  if (at("new-name").value.trim() === "") {
    return "a name is needed";
  }
  if (role() === "") {
    return "a role is needed";
  }
  return "a space is needed — or choose the whole node, which is not the same thing";
}

/** Show only the fields the chosen reach and role actually need. */
function shapeTheForm() {
  at("scope-field").hidden = at("new-reach").value === "node";
  at("role-other-field").hidden = at("new-role").value !== "other";
  preview();
}

for (const field of [
  "new-name",
  "new-scope",
  "new-role",
  "new-role-other",
  "new-password",
  "new-reach",
]) {
  at(field).addEventListener("input", shapeTheForm);
  at(field).addEventListener("change", shapeTheForm);
}

at("define").addEventListener("click", async () => {
  const statement = definition();
  if (statement === null) {
    say("define-status", missing(), true);
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

/** The role the change form describes, which may be one typed by hand. */
function changedRole() {
  const chosen = at("change-role").value;
  return chosen === "other" ? at("change-role-other").value.trim() : chosen;
}

/**
 * The `ALTER USER` this form describes, or `null` while it is incomplete.
 *
 * A password of `''` is a real password and not an empty field, so it is the
 * one input here with no emptiness check — the statement is complete the moment
 * a name is present.
 */
function alteration() {
  const name = at("change-name").value.trim();
  if (name === "") {
    return null;
  }
  if (at("change-what").value === "password") {
    return "ALTER USER " + name + " SET PASSWORD " + quoted(at("change-password").value) + ";";
  }
  const named = changedRole();
  return named === "" ? null : "ALTER USER " + name + " SET ROLE " + named + ";";
}

/**
 * Which field is still empty, named rather than left to be guessed.
 *
 * It names only what the chosen change actually needs: a hint that mentions a
 * role while somebody is typing a password reads as a second missing field, and
 * they go looking for a control that is not on screen.
 */
function changeMissing() {
  if (at("change-name").value.trim() === "") {
    return "a name is needed";
  }
  return "a role is needed";
}

/** Show the fields this change needs, and the statement it would run. */
function shapeTheChange() {
  const changing = at("change-what").value;
  at("change-password-field").hidden = changing !== "password";
  at("change-role-field").hidden = changing !== "role";
  at("change-role-other-field").hidden =
    changing !== "role" || at("change-role").value !== "other";
  const statement = alteration();
  at("change-preview").textContent =
    statement === null
      ? changeMissing()
      : // Redacted for the same reason the define form redacts: this is the one
        // place the credential would sit in plain view on somebody's screen, and
        // a shoulder is a threat a page can actually do something about.
        statement.replace(/PASSWORD '.*';$/, "PASSWORD '…';");
}

for (const field of [
  "change-name",
  "change-what",
  "change-password",
  "change-role",
  "change-role-other",
]) {
  at(field).addEventListener("input", shapeTheChange);
  at(field).addEventListener("change", shapeTheChange);
}

at("change").addEventListener("click", async () => {
  const statement = alteration();
  if (statement === null) {
    say("change-status", changeMissing(), true);
    return;
  }
  say("change-status", "running…");
  try {
    const answered = await valueOf(statement);
    say("change-status", answered !== null && answered.kind === "done" ? "ok" : "");
    // The listing carries the role, so a role change that is not redrawn leaves
    // the old one on screen looking current.
    await listUsers();
  } catch (failure) {
    say("change-status", failure.message, true);
  }
});

/**
 * The `DROP USER` this form describes, or `null` while it is not confirmed.
 *
 * The name must be typed twice and match. A single click is the wrong shape for
 * this one: the grants go with the user, a new user of the same name inherits
 * none of them, and if it was the last owner of the whole node there is no way
 * back in at all. Typing the name is the cheapest control that makes the reader
 * name who they mean.
 */
function removal() {
  const name = at("remove-name").value.trim();
  const again = at("remove-confirm").value.trim();
  return name !== "" && name === again ? "DROP USER " + name + ";" : null;
}

/** Keep the button and the preview honest about whether the two names agree. */
function shapeTheRemoval() {
  const statement = removal();
  at("remove").disabled = statement === null;
  const name = at("remove-name").value.trim();
  at("remove-preview").textContent =
    statement !== null
      ? statement
      : name === ""
        ? "a name is needed"
        : "type the same name again to confirm";
}

for (const field of ["remove-name", "remove-confirm"]) {
  at(field).addEventListener("input", shapeTheRemoval);
}

at("remove").addEventListener("click", async () => {
  const statement = removal();
  if (statement === null) {
    say("remove-status", "the two names do not match", true);
    return;
  }
  say("remove-status", "running…");
  try {
    const answered = await valueOf(statement);
    say("remove-status", answered !== null && answered.kind === "done" ? "removed" : "");
    // Cleared only on success, so a refusal leaves the name on screen to be
    // read — and never leaves a confirmed form one click from firing again.
    at("remove-name").value = "";
    at("remove-confirm").value = "";
    shapeTheRemoval();
    await listUsers();
  } catch (failure) {
    // The node's own words. A refusal here is the permission system working,
    // and paraphrasing it would hide which of the several reasons it was.
    say("remove-status", failure.message, true);
  }
});

// --------------------------------------------------------------------- node

/** One operational route, parsed as JSON, or a reason it could not be. */
async function scrape(route) {
  // Same reason as `ask`: no browser-managed credential, no native dialog.
  const reply = await fetch(route, { credentials: "omit" });
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

// ------------------------------------------------- your own password

/**
 * Keep the button honest about whether the three fields agree.
 *
 * The new one twice, because this is the one field on the page whose value
 * nobody can read back: a typo here is discovered at the next sign-in, by
 * somebody who no longer knows what they typed.
 */
function shapeMine() {
  const current = at("mine-current").value;
  const fresh = at("mine-new").value;
  const again = at("mine-again").value;
  at("mine").disabled = current === "" || fresh === "" || fresh !== again;
  say(
    "mine-status",
    fresh !== "" && again !== "" && fresh !== again ? "the two new ones differ" : "",
    fresh !== "" && again !== "" && fresh !== again,
  );
}

for (const field of ["mine-current", "mine-new", "mine-again"]) {
  at(field).addEventListener("input", shapeMine);
}

at("mine").addEventListener("click", async () => {
  const name = at("user").value.trim();
  if (name === "") {
    say("mine-status", "sign in first — this changes your own password", true);
    return;
  }
  say("mine-status", "changing…");
  try {
    // Basic and not the token this page is holding: the route asks for the
    // current password as a second proof, and a token is not one. The bytes are
    // encoded the same way `credential()` does it, for the same reason.
    const bytes = new TextEncoder().encode(name + ":" + at("mine-current").value);
    const reply = await fetch("/password", {
      method: "POST",
      headers: { Authorization: "Basic " + btoa(String.fromCharCode(...bytes)) },
      body: at("mine-new").value,
      credentials: "omit",
    });
    const text = await reply.text();
    if (reply.status >= 400) {
      say("mine-status", reason(text), true);
      return;
    }
    // Every token is dead now, this page's included, so it is signed out here
    // rather than left to fail on the next button somebody presses.
    for (const field of ["mine-current", "mine-new", "mine-again"]) {
      at(field).value = "";
    }
    shapeMine();
    at("sign-out").click();
    say("identity-status", "password changed — sign in with the new one", false);
  } catch (failure) {
    say("mine-status", "the node did not answer: " + failure.message, true);
  }
});

shapeTheForm();
shapeTheChange();
shapeTheRemoval();
shapeMine();
