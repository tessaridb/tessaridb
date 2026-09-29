//! The Topics screen's changes: a topic made or removed, a group made, moved or removed.
//!
//! Each form composes its statement from checked parts and says, in prose, what
//! pressing the button will do — the numbers in that sentence come from the
//! node's last answer about the chosen topic, so the reader sees the blast
//! radius before the button is live. The three that lose something (removing a
//! topic, removing a group, moving a group) ask for the name to be typed again.

import { Unreachable, valueOf } from "./api.js";
import { at, disable, say, setValue, trimmed, value, write } from "./dom.js";
import { told } from "./session.js";
import { held } from "./topic-info.js";
import { chosen, offer, readTopics, whenChosen, where } from "./topic-list.js";
import { aDuration, aGroup, aName, aWhole, tenancy } from "./topic-names.js";

const SCREEN = "Topics";
const MAX_IN_FLIGHT = 10_000;

/** A statement ready to send, or the sentence that says why it is not. */
type Composed = { readonly statement: string; readonly says: string } | { readonly missing: string };

const PLACE = "choose a namespace and a database first";
const TOPIC = "choose a topic in the list first";
const DURATION = "a number and a unit: 500ms, 30s, 15m, 12h, 7d";

const PLURAL = new Intl.PluralRules("en");

/** `1 message`, `2 messages` — a count and its noun, agreeing. */
const counted = (count: number, one: string, many: string): string =>
  `${count} ${PLURAL.select(count) === "one" ? one : many}`;

/** An optional field: empty is `undefined`, a bad value is `null`. */
function optional<T>(id: string, check: (text: string) => T | null): T | null | undefined {
  const text = trimmed(id);
  return text === "" ? undefined : check(text);
}

function newTopic(): Composed {
  const place = where();
  const name = aName(trimmed("new-topic-name"));
  const retain = optional("new-topic-retain", aDuration);
  const bytes = optional("new-topic-bytes", aWhole);
  if (place === null) return { missing: PLACE };
  if (name === null) return { missing: "a name: a letter or _, then letters, digits or _" };
  if (retain === null) return { missing: "keep for is " + DURATION };
  if (bytes === null || bytes === 0) return { missing: "the largest message is a whole number of bytes" };
  return {
    statement:
      `DEFINE TOPIC ${name}` + (retain === undefined ? "" : ` RETAIN ${retain}`) +
      (bytes === undefined ? "" : ` MAX BYTES ${bytes}`) + ";",
    says:
      `Creates ${name} in ${place.namespace}.${place.database}, keeping ` +
      (retain === undefined ? "every message" : `each message for ${retain}`) +
      (bytes === undefined ? "." : ` and refusing a message over ${bytes} bytes.`),
  };
}

function dropTopic(): Composed {
  const topic = chosen();
  if (where() === null) return { missing: PLACE };
  if (topic === null) return { missing: TOPIC };
  if (trimmed("drop-topic-confirm") !== topic.name) return { missing: `type ${topic.name} to confirm` };
  return {
    statement: `DROP TOPIC ${topic.name};`,
    says:
      `Removes ${topic.name} with ${counted(held(topic), "message", "messages")} it holds, ` +
      `${counted(topic.readers.size, "reader position", "reader positions")} and ` +
      `${counted(topic.groups.size, "group", "groups")}. ` +
      "Those messages are gone for every reader.",
  };
}

function newGroup(): Composed {
  const topic = chosen();
  const group = aGroup(trimmed("new-group-name"));
  const deadline = aDuration(trimmed("new-group-deadline"));
  const width = optional("new-group-width", aWhole);
  const deliveries = optional("new-group-deliveries", aWhole);
  const dead = optional("new-group-dead", aName);
  if (where() === null) return { missing: PLACE };
  if (topic === null) return { missing: TOPIC };
  if (group === null) return { missing: "a group name: letters, digits and _ . : -" };
  if (deadline === null) return { missing: "acknowledge within is " + DURATION };
  if (width === null || width === 0 || (width ?? 1) > MAX_IN_FLIGHT) {
    return { missing: `in flight is a whole number from 1 to ${MAX_IN_FLIGHT}` };
  }
  if (deliveries === null || deliveries === 0) return { missing: "give up after is a whole number of deliveries" };
  if (dead === null || dead === topic.name) return { missing: "dead letters go to another topic, by name" };
  return {
    statement:
      `DEFINE GROUP '${group}' ON TOPIC ${topic.name} ACK DEADLINE ${deadline}` +
      (deliveries === undefined ? "" : ` DELIVERIES ${deliveries}`) +
      (width === undefined ? "" : ` IN FLIGHT ${width}`) +
      (dead === undefined ? "" : ` DEAD LETTER TO ${dead}`) + ";",
    says:
      `Creates group '${group}' on ${topic.name}. Each message goes to one member and comes back ` +
      `if it is left unacknowledged for ${deadline}; ${width ?? 1} at a time` +
      (deliveries === undefined ? "" : `, given up after ${counted(deliveries, "delivery", "deliveries")}`) +
      (dead === undefined ? "" : ` and then appended to ${dead}`) + ".",
  };
}

function changeGroup(): Composed {
  const topic = chosen();
  const name = value("group-which");
  const group = topic?.groups.get(name);
  const start = aWhole(trimmed("group-start"));
  if (where() === null) return { missing: PLACE };
  if (topic === null) return { missing: TOPIC };
  if (group === undefined || aGroup(name) === null) return { missing: "the topic has no group to change" };
  if (trimmed("group-confirm") !== name) return { missing: `type ${name} to confirm` };
  if (value("group-action") === "drop") {
    return {
      statement: `DROP GROUP '${name}' ON TOPIC ${topic.name};`,
      says:
        `Removes group '${name}' and forgets its position and ` +
        `${counted(group.in_flight, "message", "messages")} it holds in flight.`,
    };
  }
  if (start === null) return { missing: "a position from 0" };
  const moved =
    start < group.position
      ? `It hands out again ${counted(group.position - start, "message", "messages")} it already handed out.`
      : start > group.position
        ? `It skips ${counted(start - group.position, "message", "messages")}.`
        : "Its position stays where it is.";
  return {
    statement: `ALTER GROUP '${name}' ON TOPIC ${topic.name} START AT ${start};`,
    says:
      `Group '${name}' next hands out position ${start + 1} and forgets ` +
      `${counted(group.in_flight, "message", "messages")} it holds in flight. ${moved}`,
  };
}

interface Form {
  readonly button: string;
  readonly says: string;
  readonly status: string;
  readonly fields: readonly string[];
  readonly compose: () => Composed;
  readonly why?: string;
}

const FORMS: readonly Form[] = [
  {
    button: "new-topic", says: "new-topic-says", status: "new-topic-status", compose: newTopic,
    fields: ["new-topic-name", "new-topic-retain", "new-topic-bytes"],
  },
  {
    button: "drop-topic", says: "drop-topic-says", status: "drop-topic-status", compose: dropTopic,
    fields: ["drop-topic-confirm", "drop-topic-why"], why: "drop-topic-why",
  },
  {
    button: "new-group", says: "new-group-says", status: "new-group-status", compose: newGroup,
    fields: ["new-group-name", "new-group-deadline", "new-group-width", "new-group-deliveries", "new-group-dead"],
  },
  {
    button: "group-apply", says: "group-says", status: "group-status", compose: changeGroup,
    fields: ["group-which", "group-action", "group-start", "group-confirm", "group-why"], why: "group-why",
  },
];

function shape(form: Form): void {
  const composed = form.compose();
  disable(form.button, !("statement" in composed));
  write(form.says, "statement" in composed ? composed.says : composed.missing);
}

async function send(form: Form): Promise<void> {
  const composed = form.compose();
  const place = where();
  if (!("statement" in composed) || place === null) {
    return;
  }
  disable(form.button, true);
  say(form.status, "sending…");
  try {
    const why = form.why === undefined ? undefined : trimmed(form.why) || undefined;
    await valueOf(tenancy(place.namespace, place.database) + composed.statement, SCREEN, why);
    say(form.status, "done");
    for (const field of form.fields) {
      if (at(field) instanceof HTMLInputElement) {
        setValue(field, field === "group-start" ? "0" : "");
      }
    }
    await readTopics();
  } catch (failure) {
    const words = told(failure);
    say(form.status, failure instanceof Unreachable ? "the node did not answer — " + words : words, true);
  }
  shape(form);
}

export function wire(): void {
  for (const form of FORMS) {
    for (const field of form.fields) {
      at(field).addEventListener("input", () => shape(form));
      at(field).addEventListener("change", () => shape(form));
    }
    at(form.button).addEventListener("click", () => void send(form));
  }
  whenChosen(() => {
    offer("group-which", [...(chosen()?.groups.keys() ?? [])].filter((name) => aGroup(name) !== null));
    for (const form of FORMS) {
      shape(form);
    }
  });
  at("topics-namespace").addEventListener("change", () => FORMS.forEach(shape));
  at("topics-database").addEventListener("change", () => FORMS.forEach(shape));
}
