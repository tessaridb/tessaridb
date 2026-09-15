// The console is a client of the public API and has no private path to it.
// Every request below is one a `curl` could make against the same node, which
// is the constraint that keeps the API the product surface rather than
// something this page sits on top of.
//
// Nothing here is fetched from anywhere else: no framework, no CDN, no web
// font. The page is meant to work on a machine with no route out at all, and a
// single remote reference would quietly take that away.

"use strict";

// The two public routes this page uses. Named once, so what the console reaches
// is greppable rather than spread through the file.
const SCRIPT_ROUTE = "/script";
const WATCH_ROUTE = "/watch";

const at = (id) => document.getElementById(id);

/**
 * The token this page is holding, or `null`.
 *
 * A password is spent once at `POST /session` and this is what comes back. It
 * lives in a variable and not in `localStorage` deliberately: a bearer token in
 * storage outlives the tab, survives the reader walking away, and is readable by
 * anything that ever manages to run script on this origin. Closing the tab
 * should end the session, and here it does.
 */
let held = null;

/** The `Authorization` value to send, or nothing when there is nothing to send. */
function credential() {
  // The token wins whenever there is one. It costs the node a hash-map lookup
  // where a password costs nineteen mebibytes of Argon2, which is the whole
  // reason `POST /session` exists.
  if (held !== null) {
    return "Bearer " + held;
  }
  return typed();
}

/** A `Basic` value for what is in the two fields, or nothing when both are empty. */
function typed() {
  const user = at("user").value;
  const password = at("password").value;
  if (user === "" && password === "") {
    return null;
  }
  // `btoa` throws on any character above U+00FF, so a password with an accent
  // in it would break the button rather than be refused by the node — and the
  // node decodes the header as UTF-8, so it would have accepted one. Encode
  // first, then base64 the bytes.
  const bytes = new TextEncoder().encode(user + ":" + password);
  return "Basic " + btoa(String.fromCharCode(...bytes));
}

/** Say something in a status line, marking a failure as one. */
function say(id, words, failed) {
  const line = at(id);
  line.textContent = words;
  line.classList.toggle("failed", failed === true);
}

/** Run a script against the node, and hand back the reply and its text. */
async function ask(source) {
  const headers = {};
  const offered = credential();
  if (offered !== null) {
    headers["Authorization"] = offered;
  }
  const reply = await fetch(SCRIPT_ROUTE, {
    method: "POST",
    headers: headers,
    body: source,
    // Without this the browser handles the node's `401` challenge itself and
    // opens its own credential dialog on top of the page — a second sign-in
    // this console did not ask for, cannot read and cannot clear, and which
    // leaves the page's own request hanging behind it. The credential is in the
    // header above; nothing here wants the browser to manage one.
    credentials: "omit",
  });
  // A token that stopped working stopped for a reason worth acting on: somebody
  // rotated the password, changed the role, or removed the account. Holding on
  // to it would make every button afterwards fail with the same refusal and none
  // of them say why.
  if (reply.status === 401 && held !== null) {
    held = null;
    signedIn();
    say("identity-status", "this session ended — sign in again", true);
  }
  return { reply: reply, text: await reply.text() };
}

// ------------------------------------------------------------------- tabs

// Hash routing, so a section is a link somebody can send and a refresh keeps
// you where you were. A panel nobody can link to is a panel people describe to
// each other in words.
const tabs = () => Array.from(document.querySelectorAll('[role="tab"]'));

function show(name) {
  const wanted = tabs().some((tab) => tab.id === "tab-" + name) ? name : "query";
  for (const tab of tabs()) {
    const chosen = tab.id === "tab-" + wanted;
    tab.setAttribute("aria-selected", String(chosen));
    tab.tabIndex = chosen ? 0 : -1;
    at(tab.getAttribute("aria-controls")).hidden = !chosen;
  }
  if (window.location.hash !== "#" + wanted) {
    window.location.hash = wanted;
  }
}

for (const tab of tabs()) {
  tab.addEventListener("click", () => show(tab.id.replace("tab-", "")));
  // Arrow keys move between tabs, which is what a tablist owes anybody not
  // using a mouse — the roles alone promise it and do not provide it.
  tab.addEventListener("keydown", (event) => {
    const step = event.key === "ArrowRight" ? 1 : event.key === "ArrowLeft" ? -1 : 0;
    if (step === 0) {
      return;
    }
    event.preventDefault();
    const all = tabs();
    const here = all.indexOf(tab);
    const next = all[(here + step + all.length) % all.length];
    next.focus();
    show(next.id.replace("tab-", ""));
  });
}

window.addEventListener("hashchange", () =>
  show(window.location.hash.replace("#", "")),
);

// --------------------------------------------------------------- identity

/** Keep the collapsed identity control honest about whether there is a session. */
function signedIn() {
  const user = at("user").value;
  if (held !== null && user !== "") {
    at("signed-in").textContent = user;
    return;
  }
  // A name typed but not yet exchanged for a token is not a session, and saying
  // so is the difference between "this will work" and "this might".
  at("signed-in").textContent = user === "" ? "not signed in" : user + " — not yet";
}

at("user").addEventListener("input", signedIn);

/** The disclosure this control lives in, closed by everything that should. */
const identity = document.querySelector("details.identity");

/** The `error` out of a refusal's body, or the body when it is not one. */
function reason(text) {
  try {
    const body = JSON.parse(text);
    return typeof body.error === "string" ? body.error : text;
  } catch (ignored) {
    // Not JSON, so the text is already the most useful thing there is.
    return text;
  }
}

/**
 * Spend the password once, keep the token, and close the sheet.
 *
 * This is a real sign-in and not a check any more: the node verifies the
 * password here and hands back a token, and every request after this one
 * carries the token instead. The password is then **cleared from the field**,
 * because there is nothing left that needs it and a credential sitting in a DOM
 * input is a credential in every screenshot, screen share and browser
 * extension that reads the page.
 */
at("sign-in").addEventListener("click", async () => {
  const offered = typed();
  if (offered === null) {
    say("identity-status", "a name and a password, or nothing at all", true);
    return;
  }
  say("identity-status", "signing in…");
  try {
    const reply = await fetch("/session", {
      method: "POST",
      headers: { Authorization: offered },
      // The same reason `ask` omits them: left to itself the browser answers
      // the node's `401` challenge with its own credential dialog, which this
      // console did not ask for and cannot clear.
      credentials: "omit",
    });
    const text = await reply.text();
    if (reply.status >= 400) {
      say("identity-status", reason(text), true);
      return;
    }
    held = JSON.parse(text).token;
    at("password").value = "";
    say("identity-status", "");
    signedIn();
    identity.open = false;
  } catch (failure) {
    say("identity-status", "the node did not answer: " + failure.message, true);
  }
});

/**
 * Hand the token back, then forget it here.
 *
 * Told to the node rather than only dropped locally: a token this page forgets
 * without saying so stays live on the node until it expires, which is the
 * difference between signing out and closing your eyes.
 */
at("sign-out").addEventListener("click", async () => {
  if (held !== null) {
    try {
      await fetch("/session", {
        method: "DELETE",
        headers: { Authorization: "Bearer " + held },
        credentials: "omit",
      });
    } catch (ignored) {
      // The node is unreachable. Forgetting it here is still right — the token
      // expires on its own, and staying signed in because the network failed is
      // the wrong way to be wrong.
    }
  }
  held = null;
  at("user").value = "";
  at("password").value = "";
  say("identity-status", "");
  signedIn();
  identity.open = false;
});

// A sheet that hangs over the page until something else is clicked is the
// complaint this control earned. Escape and a click outside both close it,
// which is what every other disclosure on the web does.
identity.addEventListener("keydown", (event) => {
  if (event.key === "Escape") {
    identity.open = false;
  }
});

document.addEventListener("click", (event) => {
  if (identity.open && !identity.contains(event.target)) {
    identity.open = false;
  }
});

// ---------------------------------------------------------- drawing an answer

/** Whether a value belongs in a cell: not an object, not an array. */
function flat(value) {
  return value === null || typeof value !== "object";
}

/**
 * The fields every record shares, or `null` when they do not share a shape.
 *
 * The same rule the terminal follows, and for the same reason. A union of the
 * field sets with blanks where a record has none would table more answers and
 * would make *absent* and *empty* look identical — in the rendering, which is
 * the last place a distinction should be lost.
 */
function shape(records) {
  let agreed = null;
  for (const record of records) {
    const held = record.value;
    if (held === null || typeof held !== "object" || Array.isArray(held)) {
      return null;
    }
    const here = Object.keys(held).sort();
    if (!here.every((field) => flat(held[field]))) {
      return null;
    }
    if (agreed === null) {
      agreed = here;
    } else if (agreed.length !== here.length || !agreed.every((f, i) => f === here[i])) {
      return null;
    }
  }
  return agreed !== null && agreed.length > 0 ? agreed : null;
}

/** One cell's text. Numbers stay numbers; everything else is JSON. */
function cell(value) {
  return typeof value === "string" ? value : JSON.stringify(value);
}

/** These records as a table element, or `null` if they are not one. */
function drawn(records) {
  const fields = shape(records);
  if (fields === null) {
    return null;
  }
  const numeric = fields.map((field) =>
    records.every((record) => typeof record.value[field] === "number"),
  );

  const table = document.createElement("table");
  const head = table.createTHead().insertRow();
  for (const name of ["id", ...fields]) {
    const column = document.createElement("th");
    column.textContent = name;
    head.appendChild(column);
  }
  const body = table.createTBody();
  for (const record of records) {
    const row = body.insertRow();
    // `textContent` throughout: these values come out of the store, and a record
    // that happens to hold a `<script>` tag is data, not markup.
    row.insertCell().textContent = record.id;
    fields.forEach((field, index) => {
      const box = row.insertCell();
      box.textContent = cell(record.value[field]);
      if (numeric[index]) {
        box.classList.add("number");
      }
    });
  }
  return table;
}

/** How the answer pane is drawing things: by shape, or as the JSON that came. */
let drawing = "auto";

/** The parsed body of the last answer, so the toggle can redraw without asking. */
let answered = null;

function paint() {
  const pane = at("answer");
  pane.textContent = "";
  if (answered === null) {
    return;
  }
  if (drawing === "json" || !Array.isArray(answered.results)) {
    const shown = document.createElement("pre");
    shown.textContent = JSON.stringify(answered, null, 2);
    pane.appendChild(shown);
    return;
  }
  for (const result of answered.results) {
    if (result.kind === "records" && Array.isArray(result.records)) {
      const table = result.records.length === 0 ? null : drawn(result.records);
      if (table === null) {
        const shown = document.createElement("pre");
        shown.textContent =
          result.records.length === 0
            ? "(no records)"
            : JSON.stringify(result.records, null, 2);
        pane.appendChild(shown);
      } else {
        pane.appendChild(table);
      }
      // The trailer says how many and by which path. A scan should be visible
      // rather than folklore, which is why the store reports the path at all.
      const trailer = document.createElement("p");
      trailer.className = "trailer";
      trailer.textContent =
        "(" + result.records.length + " record(s), via " + result.path + ")";
      pane.appendChild(trailer);
    } else if (result.kind === "done") {
      // What the terminal prints for the same answer, and for the same reason:
      // a script's `USE` and `DEFINE` statements each answer, and three lines of
      // JSON apiece would bury the result somebody actually ran the script for.
      const shown = document.createElement("p");
      shown.className = "trailer";
      shown.textContent = "ok";
      pane.appendChild(shown);
    } else {
      const shown = document.createElement("pre");
      shown.textContent = JSON.stringify(result, null, 2);
      pane.appendChild(shown);
    }
  }
}

for (const button of document.querySelectorAll("[data-shape]")) {
  button.addEventListener("click", () => {
    drawing = button.dataset.shape;
    for (const other of document.querySelectorAll("[data-shape]")) {
      other.classList.toggle("chosen", other === button);
    }
    paint();
  });
}

// ------------------------------------------------------------- run a script

async function runScript() {
  say("script-status", "running…");
  answered = null;
  at("answer").textContent = "";
  try {
    const { reply, text } = await ask(at("script").value);
    // Parsed when it is JSON and shown as it came when it is not: an error body
    // is plain text and reformatting it would only hide it.
    try {
      answered = JSON.parse(text);
    } catch (ignored) {
      answered = null;
      const shown = document.createElement("pre");
      shown.textContent = text;
      at("answer").appendChild(shown);
    }
    paint();
    say("script-status", reply.status + " " + reply.statusText, reply.status >= 400);
  } catch (failure) {
    // A fetch rejects only when the request never got an answer, so this is a
    // connection problem and never a refusal from the node.
    say("script-status", "the node did not answer: " + failure.message, true);
  }
}

at("run").addEventListener("click", runScript);

at("script").addEventListener("keydown", (event) => {
  if (event.key === "Enter" && (event.metaKey || event.ctrlKey)) {
    event.preventDefault();
    runScript();
  }
});

// ------------------------------------------------------------ watch a table

let following = null;

function stopFollowing(words) {
  if (following !== null) {
    following.close();
    following = null;
  }
  at("follow").disabled = false;
  at("stop").disabled = true;
  if (words !== undefined) {
    say("watch-status", words);
  }
}

/** Add one change to the top of the list, as text and never as markup. */
function change(what) {
  const line = document.createElement("li");
  const became = typeof what.became === "string" ? what.became : "";
  line.classList.add(became === "removed" ? "removed" : "written");
  line.textContent =
    "#" +
    what.sequence +
    "  " +
    what.table +
    ":" +
    what.id +
    "  " +
    became +
    (what.value === undefined ? "" : "  " + JSON.stringify(what.value));
  const list = at("changes");
  list.insertBefore(line, list.firstChild);
}

at("follow").addEventListener("click", () => {
  stopFollowing();
  at("changes").textContent = "";

  // Resolved against this page's own address rather than assembled from pieces:
  // same origin by construction, and it leaves the route a plain readable
  // literal instead of a fragment glued to a scheme. The scheme follows the
  // page's own, because a console served over TLS must not open a plaintext
  // socket.
  const where = new URL(WATCH_ROUTE, window.location.href);
  where.protocol = window.location.protocol === "https:" ? "wss:" : "ws:";
  const socket = new WebSocket(where);
  following = socket;
  at("follow").disabled = true;
  at("stop").disabled = false;
  say("watch-status", "connecting…");

  socket.addEventListener("open", () => {
    const asked = {
      namespace: at("namespace").value,
      database: at("database").value,
      from: Number(at("from").value),
    };
    const table = at("table").value;
    if (table !== "") {
      asked.table = table;
    }
    // A browser cannot set a header on a `WebSocket`, so whatever authenticates
    // this travels in the message. A token when there is one — it expires and
    // can be revoked, which a password does neither of, and it means following
    // a table does not put a password into a message body.
    if (held !== null) {
      asked.token = held;
    } else {
      // Both halves or neither: a name without a password would be asking to be
      // signed in without proof.
      const user = at("user").value;
      const password = at("password").value;
      if (user !== "" || password !== "") {
        asked.user = user;
        asked.password = password;
      }
    }
    socket.send(JSON.stringify({ ...asked }));
    say("watch-status", "following");
  });

  socket.addEventListener("message", (event) => {
    let what = null;
    try {
      what = JSON.parse(event.data);
    } catch (ignored) {
      say("watch-status", "the node sent something this page cannot read", true);
      return;
    }
    // The node answers a request it will not serve in words rather than by
    // going quiet, so an operator can tell "not permitted" from "nothing has
    // happened yet".
    if (typeof what.refused === "string") {
      say("watch-status", what.refused, true);
      return;
    }
    if (typeof what.error === "string") {
      say("watch-status", what.error, true);
      return;
    }
    change(what);
  });

  socket.addEventListener("close", (event) => {
    // 1001 is this node stopping. The position is held by the client, so
    // following again from the last sequence seen resumes exactly there.
    stopFollowing(event.code === 1001 ? "the node is stopping" : "stopped");
  });

  socket.addEventListener("error", () => {
    say("watch-status", "the socket failed", true);
  });
});

at("stop").addEventListener("click", () => stopFollowing("stopped"));

at("where").textContent = "served by " + window.location.host;
signedIn();
show(window.location.hash.replace("#", ""));
