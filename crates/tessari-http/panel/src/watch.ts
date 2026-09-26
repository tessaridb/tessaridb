//! Following a table as it changes.

import { WATCH_ROUTE } from "./api.js";
import { at, clear, disable, made, say, value } from "./dom.js";
import { token } from "./session.js";

/** The socket this page is holding open, or `null`. */
let following: WebSocket | null = null;

/**
 * Whether the node has already said why this follow is ending.
 *
 * A refusal arrives as a message and the socket closes immediately after it, so
 * the close handler's own word overwrote the reason a moment after showing it.
 * Following a namespace that does not exist ended on `stopped` — the same word,
 * to the byte, that the Stop button writes. Measured in W320 as an A/B on one
 * field: a refused follow and an operator's own stop were indistinguishable.
 */
let toldWhy = false;

/**
 * Whether a follow is running right now.
 *
 * Asked on the way out, so a reload can say what was actually lost instead of
 * guessing from a field that happened to have text in it.
 */
export const isFollowing = (): boolean => following !== null;

/** What the node sends for one change, or instead of one. */
interface Change {
  readonly sequence?: number;
  readonly table?: string;
  readonly id?: string;
  readonly became?: string;
  readonly value?: unknown;
  readonly cursor?: string;
  readonly refused?: string;
  readonly error?: string;
}

/** What this page asks the node to follow. */
interface Asked {
  namespace: string;
  database: string;
  from: number;
  table?: string;
  cursor?: string;
  token?: string;
  user?: string;
  password?: string;
}

function stop(words?: string): void {
  if (following !== null) {
    following.close();
    following = null;
  }
  disable("follow", false);
  disable("stop", true);
  if (words !== undefined) {
    say("watch-status", words);
  }
}

/** Add one change to the top of the list, as text and never as markup. */
function change(what: Change): void {
  const line = made("li");
  const became = typeof what.became === "string" ? what.became : "";
  line.classList.add(became === "removed" ? "removed" : "written");
  line.textContent =
    "#" +
    String(what.sequence) +
    "  " +
    String(what.table) +
    ":" +
    String(what.id) +
    "  " +
    became +
    (what.value === undefined ? "" : "  " + JSON.stringify(what.value)) +
    (what.cursor === undefined ? "" : "  cursor " + what.cursor);
  // A split table's changes are counted per log, so the resume point is the
  // cursor rather than the sequence — kept in the field following again sends.
  if (typeof what.cursor === "string") {
    (at("cursor") as HTMLInputElement).value = what.cursor;
  }
  const list = at("changes");
  list.insertBefore(line, list.firstChild);
}

/** The socket's address: this page's own, with the scheme it was served over. */
function where(): URL {
  // Resolved against this page's own address rather than assembled from pieces:
  // same origin by construction, and it leaves the route a plain readable
  // literal instead of a fragment glued to a scheme. The scheme follows the
  // page's own, because a console served over TLS must not open a plaintext
  // socket.
  const address = new URL(WATCH_ROUTE, window.location.href);
  address.protocol = window.location.protocol === "https:" ? "wss:" : "ws:";
  return address;
}

/** What authenticates this subscription, put into the message body. */
function asked(): Asked {
  const wanted: Asked = {
    namespace: value("namespace"),
    database: value("database"),
    from: Number(value("from")),
  };
  const table = value("table");
  if (table !== "") {
    wanted.table = table;
  }
  const cursor = value("cursor");
  if (cursor !== "") {
    wanted.cursor = cursor;
  }
  // A browser cannot set a header on a `WebSocket`, so whatever authenticates
  // this travels in the message. A token when there is one — it expires and can
  // be revoked, which a password does neither of, and it means following a
  // table does not put a password into a message body.
  const carried = token();
  if (carried !== null) {
    wanted.token = carried;
    return wanted;
  }
  // Both halves or neither: a name without a password would be asking to be
  // signed in without proof.
  const user = value("user");
  const password = value("password");
  if (user !== "" || password !== "") {
    wanted.user = user;
    wanted.password = password;
  }
  return wanted;
}

export function wire(): void {
  at("follow").addEventListener("click", () => {
    stop();
    clear("changes");

    const socket = new WebSocket(where());
    following = socket;
    toldWhy = false;
    disable("follow", true);
    disable("stop", false);
    say("watch-status", "connecting…");

    socket.addEventListener("open", () => {
      socket.send(JSON.stringify(asked()));
      say("watch-status", "following");
    });

    socket.addEventListener("message", (event) => {
      let what: Change;
      try {
        what = JSON.parse(String(event.data)) as Change;
      } catch {
        say("watch-status", "the node sent something this page cannot read", true);
        return;
      }
      // The node answers a request it will not serve in words rather than by
      // going quiet, so an operator can tell "not permitted" from "nothing has
      // happened yet".
      if (typeof what.refused === "string") {
        say("watch-status", what.refused, true);
        toldWhy = true;
        return;
      }
      if (typeof what.error === "string") {
        say("watch-status", what.error, true);
        toldWhy = true;
        return;
      }
      change(what);
    });

    socket.addEventListener("close", (event) => {
      // 1001 is this node stopping. The position is held by the client, so
      // following again from the last sequence seen — or, over a split table,
      // the last cursor, which the field already holds — resumes exactly there.
      //
      // A reason already given is left standing: the node explains a refusal in
      // words and then closes, and replacing those words with `stopped` throws
      // away the only account of what happened. The buttons still return.
      stop(toldWhy ? undefined : event.code === 1001 ? "the node is stopping" : "stopped");
    });

    socket.addEventListener("error", () => {
      say("watch-status", "the socket failed", true);
    });
  });

  at("stop").addEventListener("click", () => stop("stopped"));
}
