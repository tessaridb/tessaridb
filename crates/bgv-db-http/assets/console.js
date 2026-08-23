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

/** The `Authorization` value for what is typed, or nothing when both are empty. */
function credential() {
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

// ------------------------------------------------------------- run a script

at("run").addEventListener("click", async () => {
  const source = at("script").value;
  const headers = {};
  const offered = credential();
  if (offered !== null) {
    headers["Authorization"] = offered;
  }
  say("script-status", "running…");
  at("answer").textContent = "";
  try {
    const reply = await fetch(SCRIPT_ROUTE, {
      method: "POST",
      headers: headers,
      body: source,
    });
    const text = await reply.text();
    // Pretty-print when it is JSON and show it as it came when it is not: an
    // error body is plain text and reformatting it would only hide it.
    let shown = text;
    try {
      shown = JSON.stringify(JSON.parse(text), null, 2);
    } catch (ignored) {
      shown = text;
    }
    at("answer").textContent = shown;
    say("script-status", reply.status + " " + reply.statusText, reply.status >= 400);
  } catch (failure) {
    // A fetch rejects only when the request never got an answer, so this is a
    // connection problem and never a refusal from the node.
    say("script-status", "the node did not answer: " + failure.message, true);
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
function show(change) {
  const line = document.createElement("li");
  const became = typeof change.became === "string" ? change.became : "";
  line.classList.add(became === "removed" ? "removed" : "written");
  // `textContent` throughout: these values come out of the store, and a record
  // that happens to hold a `<script>` tag is data, not markup.
  line.textContent =
    "#" +
    change.sequence +
    "  " +
    change.table +
    ":" +
    change.id +
    "  " +
    became +
    (change.value === undefined ? "" : "  " + JSON.stringify(change.value));
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
    // A browser cannot set a header on a `WebSocket`, so the credential travels
    // in the message when there is one. Both halves or neither: a name without
    // a password would be asking to be signed in without proof.
    const user = at("user").value;
    const password = at("password").value;
    if (user !== "" || password !== "") {
      asked.user = user;
      asked.password = password;
    }
    socket.send(JSON.stringify({ ...asked }));
    say("watch-status", "following");
  });

  socket.addEventListener("message", (event) => {
    let change = null;
    try {
      change = JSON.parse(event.data);
    } catch (ignored) {
      say("watch-status", "the node sent something this page cannot read", true);
      return;
    }
    // The node answers a request it will not serve in words rather than by
    // going quiet, so an operator can tell "not permitted" from "nothing has
    // happened yet".
    if (typeof change.refused === "string") {
      say("watch-status", change.refused, true);
      return;
    }
    if (typeof change.error === "string") {
      say("watch-status", change.error, true);
      return;
    }
    show(change);
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
