//! Resource readings, as the console renders them.
//!
//! What `INFO FOR NODE` now answers of a machine — its own, and each bound
//! peer's as this node last heard it. Every number keeps the two labels the
//! engine puts on it: `observed` (how old the sample is) and `stale` (whether
//! the freshness bound the cluster's policy sets still admits it). A stale
//! reading is drawn as stale, never silently as the present — that is the one
//! thing this module must never do.
//!
//! A figure the node could not measure answers `null`, and renders as "not
//! reported" rather than a zero: absence and zero are different claims, and
//! the console does not get to pick the more comfortable one.

/**
 * One set of readings, as `resources` on the self answer or `readings` on a
 * peer row carries it. Every field is the engine's own spelling.
 */
export interface Readings {
    readonly cpu_cores?: number | null;
    /** Basis points of one percent (0..=10_000). */
    readonly cpu_busy?: number | null;
    readonly memory_total?: number | null;
    readonly memory_used?: number | null;
    readonly disk_total?: number | null;
    readonly disk_free?: number | null;
    /** Bytes per second over the sampling window. */
    readonly network_received?: number | null;
    readonly network_transmitted?: number | null;
    /** Whole seconds, rounded up, since the sample was taken. */
    readonly observed?: number | null;
    readonly stale?: boolean;
}

/** Bytes as a person reads them: one decimal, the largest unit that fits. */
function bytes(held: number): string {
    const units = ["B", "KB", "MB", "GB", "TB"] as const;
    let value = held;
    let unit = 0;
    while (value >= 1024 && unit < units.length - 1) {
        value /= 1024;
        unit += 1;
    }
    return `${unit === 0 ? value : value.toFixed(1)} ${units[unit]}`;
}

/** Basis points as a percentage with one decimal, "not reported" when absent. */
function percent(points: number | null | undefined): string | null {
    return typeof points === "number" ? `${(points / 100).toFixed(1)}%` : null;
}

/** One throughput figure, `null` when the node could not measure it. */
function rate(perSecond: number | null | undefined): string | null {
    return typeof perSecond === "number" ? `${bytes(perSecond)}/s` : null;
}

/**
 * The freshness sentence: how old the sample is, and — when it is past the
 * bound — the word `stale` where an operator scanning at 03:00 will see it.
 * `null` when the engine reported no sample at all, which is a different
 * answer from a fresh zero.
 */
export function freshness(held: Readings | null | undefined): string | null {
    if (held === null || held === undefined) {
        return null;
    }
    const observed = typeof held.observed === "number" ? held.observed : null;
    if (observed === null) {
        return "no sample held";
    }
    const when = observed === 0 ? "just now" : `${observed}s ago`;
    return held.stale === true
        ? `stale — observed ${when}, past the freshness bound`
        : `observed ${when}`;
}

/**
 * The six figures as lines, for a facts table. Absent figures are named, not
 * skipped: a row that is there for some servers and silently missing for
 * others teaches the reader that the column lies.
 */
export function resourceLines(held: Readings | null | undefined): string {
    if (held === null || held === undefined) {
        return "no readings held";
    }
    const cores =
        typeof held.cpu_cores === "number"
            ? `${held.cpu_cores} core${held.cpu_cores === 1 ? "" : "s"}`
            : "cores not reported";
    const busy = percent(held.cpu_busy);
    const used =
        typeof held.memory_used === "number" && typeof held.memory_total === "number"
            ? `${bytes(held.memory_used)} of ${bytes(held.memory_total)} used`
            : "memory not reported";
    const disk =
        typeof held.disk_free === "number" && typeof held.disk_total === "number"
            ? `${bytes(held.disk_free)} of ${bytes(held.disk_total)} free`
            : "disk not reported";
    const net =
        rate(held.network_received) !== null && rate(held.network_transmitted) !== null
            ? `in ${rate(held.network_received)}, out ${rate(held.network_transmitted)}`
            : "network not reported";
    const fresh = freshness(held);
    return [
        busy === null ? cores : `${cores} · ${busy} busy`,
        used,
        disk,
        net,
        fresh ?? "no sample held",
    ].join("\n");
}

/**
 * One line for a map card: the figures an operator scans for at a glance.
 * The full set lives in the drawer; the card carries load and disk, the two
 * numbers a cluster overview is read for.
 */
export function resourceSummary(held: Readings | null | undefined): string | null {
    if (held === null || held === undefined) {
        return null;
    }
    const parts: string[] = [];
    const busy = percent(held.cpu_busy);
    if (busy !== null) {
        parts.push(`${busy} busy`);
    }
    if (typeof held.memory_used === "number" && typeof held.memory_total === "number" && held.memory_total > 0) {
        parts.push(`${Math.round((held.memory_used / held.memory_total) * 100)}% memory`);
    }
    if (typeof held.disk_free === "number" && typeof held.disk_total === "number" && held.disk_total > 0) {
        parts.push(`${Math.round((held.disk_free / held.disk_total) * 100)}% disk free`);
    }
    if (parts.length === 0) {
        return null;
    }
    const fresh = freshness(held);
    const said = parts.join(" · ");
    return fresh === null ? said : `${said} (${fresh})`;
}
