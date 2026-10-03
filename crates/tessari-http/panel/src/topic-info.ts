//! What the node says about a topic, checked before anything draws it.
//!
//! `INFO FOR TOPIC` is a body this page did not construct, so each field is
//! narrowed here once and the screens after this point trust the shape.

/** A reader under a name with no group: one position, and how far behind it is. */
export interface Reader {
  readonly position: number;
  readonly lag: number;
}

/** A consumer group, in the node's own field names. */
export interface Group {
  readonly position: number;
  readonly committed: number;
  readonly lag: number;
  readonly in_flight: number;
  readonly redelivered: number;
  readonly dead_lettered: number;
  readonly deadline: string;
  readonly width: number;
}

/** A topic consumer reading this topic into a table (ADR-0087). */
export interface Ingest {
  readonly group: string;
  /** The table, or `hidden` when the caller may not read it. */
  readonly into: string;
  /** Running on the node that answered. */
  readonly running: boolean;
}

export interface Topic {
  readonly name: string;
  /** Absent while the topic holds nothing. */
  readonly first: number | null;
  readonly last: number;
  readonly retain: string | null;
  /** `RETAIN BYTES n`: the most bytes of messages the topic keeps. */
  readonly retainBytes: number | null;
  /** The bytes it holds now — reported only beside `retainBytes`. */
  readonly bytes: number | null;
  readonly maxBytes: number | null;
  readonly readers: ReadonlyMap<string, Reader>;
  readonly groups: ReadonlyMap<string, Group>;
  readonly ingestedBy: ReadonlyMap<string, Ingest>;
}

const whole = (value: unknown): number | null =>
  typeof value === "number" && Number.isInteger(value) && value >= 0 ? value : null;

export const fieldsOf = (value: unknown): Record<string, unknown> | null =>
  typeof value === "object" && value !== null && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : null;

function reader(value: unknown): Reader | null {
  const fields = fieldsOf(value);
  const position = whole(fields?.["position"]);
  const lag = whole(fields?.["lag"]);
  return position === null || lag === null ? null : { position, lag };
}

function group(value: unknown): Group | null {
  const fields = fieldsOf(value);
  const at = (name: string): number | null => whole(fields?.[name]);
  const position = at("position");
  const committed = at("committed");
  const lag = at("lag");
  const inFlight = at("in_flight");
  const redelivered = at("redelivered");
  const deadLettered = at("dead_lettered");
  const width = at("width");
  const deadline = fields?.["deadline"];
  if (
    position === null || committed === null || lag === null || inFlight === null ||
    redelivered === null || deadLettered === null || width === null ||
    typeof deadline !== "string"
  ) {
    return null;
  }
  return {
    position, committed, lag, in_flight: inFlight, redelivered,
    dead_lettered: deadLettered, deadline, width,
  };
}

function ingest(value: unknown): Ingest | null {
  const fields = fieldsOf(value);
  const group = fields?.["group"];
  const into = fields?.["into"];
  const running = fields?.["running"];
  return typeof group === "string" && typeof into === "string" && typeof running === "boolean"
    ? { group, into, running }
    : null;
}

/** Each entry of a name-keyed object that narrows, in the node's order. */
function entries<T>(value: unknown, narrow: (each: unknown) => T | null): Map<string, T> {
  const found = new Map<string, T>();
  for (const [name, each] of Object.entries(fieldsOf(value) ?? {})) {
    const narrowed = narrow(each);
    if (narrowed !== null) {
      found.set(name, narrowed);
    }
  }
  return found;
}

/** A topic out of an `INFO FOR TOPIC` value, or `null` when it is not one. */
export function topic(value: unknown): Topic | null {
  const fields = fieldsOf(value);
  const name = fields?.["name"];
  const last = whole(fields?.["last"]);
  if (fields === null || typeof name !== "string" || last === null) {
    return null;
  }
  const retain = fields["retain"];
  return {
    name,
    first: whole(fields["first"]),
    last,
    retain: typeof retain === "string" ? retain : null,
    retainBytes: whole(fields["retain_bytes"]),
    bytes: whole(fields["bytes"]),
    maxBytes: whole(fields["max_bytes"]),
    readers: entries(fields["consumers"], reader),
    groups: entries(fields["groups"], group),
    ingestedBy: entries(fields["ingested_by"], ingest),
  };
}

/** What the topic keeps: a time, a size, both, or everything. */
export function keeps(topic: Topic): string {
  const limits = [
    ...(topic.retain === null ? [] : [topic.retain]),
    ...(topic.retainBytes === null ? [] : [`${topic.retainBytes} bytes`]),
  ];
  return limits.length === 0 ? "everything" : limits.join(", ");
}

/** How many messages the topic holds right now — positions are dense. */
export const held = (topic: Topic): number =>
  topic.first === null || topic.last < topic.first ? 0 : topic.last - topic.first + 1;

/** The reader or group furthest behind, in messages. */
export function behind(topic: Topic): number {
  const lags = [...topic.readers.values(), ...topic.groups.values()].map((each) => each.lag);
  return lags.length === 0 ? 0 : Math.max(...lags);
}
