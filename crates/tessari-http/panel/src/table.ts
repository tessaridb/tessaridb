//! The servers table — the cluster at grown-cluster density.
//!
//! The map is the signature element and stays the first thing on the pane;
//! this table is what a thousand servers are worked from: find by name,
//! address or id, sort on every column, filter by status and by resource
//! bounds, and open the same drawer the map card opens. It reads the same
//! `INFO FOR NODE` answer the map draws — nothing here is inferred, and each
//! reading keeps the age and stale label the engine puts on it.
//!
//! The filters are the console's own for now; the wave that adds `$nodes` to
//! the language turns them into statements without this screen changing
//! shape.

import { made } from "./dom.js";
import { show, type Subject } from "./drawer.js";
import { subjects, type Seen } from "./map.js";
import { type Readings } from "./readings.js";

/** One row: what the answer carries about one server, pre-digested for sort. */
interface Row {
    readonly subject: Subject;
    readonly name: string;
    readonly address: string | null;
    readonly roles: readonly string[];
    readonly region: string | null;
    readonly leads: string | null;
    /** Fractions 0..=1, `null` when the engine reported no usable figure. */
    readonly diskFree: number | null;
    readonly memoryUsed: number | null;
    readonly cpuBusy: number | null;
    readonly stale: boolean;
    readonly observed: number | null;
    readonly searchable: string;
}

const COLUMNS = [
    { key: "name", label: "server" },
    { key: "address", label: "address" },
    { key: "roles", label: "roles" },
    { key: "region", label: "region" },
    { key: "leads", label: "leads" },
    { key: "disk", label: "disk free" },
    { key: "memory", label: "memory used" },
    { key: "cpu", label: "cpu busy" },
    { key: "fresh", label: "readings" },
] as const;

type ColumnKey = (typeof COLUMNS)[number]["key"];

/** The filter and sort state, kept across reads so a refresh is not an erase. */
const wanted: {
    find: string;
    status: string;
    bound: string;
    sort: ColumnKey;
    dir: 1 | -1;
} = {
    find: "",
    status: "all",
    bound: "any",
    sort: "name",
    dir: 1,
};

const STATUS_FILTERS = [
    { value: "all", label: "all" },
    { value: "drained", label: "drained" },
    { value: "serving", label: "serving" },
    { value: "writable", label: "writable" },
    { value: "coordinating", label: "coordinating" },
];

const BOUND_FILTERS = [
    { value: "any", label: "any load" },
    { value: "disk25", label: "disk free < 25%" },
    { value: "disk10", label: "disk free < 10%" },
    { value: "mem85", label: "memory used > 85%" },
    { value: "cpu80", label: "cpu busy > 80%" },
    { value: "stale", label: "stale readings" },
];

function fraction(
    part: number | null | undefined,
    whole: number | null | undefined,
): number | null {
    return typeof part === "number" && typeof whole === "number" && whole > 0
        ? part / whole
        : null;
}

function rowOf(subject: Subject, readings: Readings | null | undefined): Row {
    const observed = typeof readings?.observed === "number" ? readings.observed : null;
    return {
        subject,
        name: subject.name,
        address: subject.endpoint,
        roles: subject.roles,
        region:
            typeof subject.about["region"] === "string"
                ? (subject.about["region"] as string)
                : null,
        leads:
            typeof subject.about["leads, as the log records"] === "string"
                ? (subject.about["leads, as the log records"] as string)
                : null,
        diskFree: fraction(readings?.disk_free, readings?.disk_total),
        memoryUsed: fraction(readings?.memory_used, readings?.memory_total),
        cpuBusy: fraction(readings?.cpu_busy, 10_000),
        stale: readings?.stale === true,
        observed,
        searchable: [
            subject.name,
            subject.endpoint ?? "",
            subject.node ?? "",
            subject.about["id"] ?? "",
        ]
            .join(" ")
            .toLowerCase(),
    };
}

function rowsOf(seen: Seen): Row[] {
    const roster = subjects(seen);
    const peers = seen.cluster?.peers ?? [];
    const out: Row[] = [rowOf(roster.self, seen.resources ?? null)];
    roster.peers.forEach((subject, at) => {
        out.push(rowOf(subject, peers[at]?.readings ?? null));
    });
    return out;
}

function kept(row: Row): boolean {
    if (wanted.find !== "" && !row.searchable.includes(wanted.find)) {
        return false;
    }
    if (wanted.status === "drained") {
        if (row.roles.length !== 0) {
            return false;
        }
    } else if (wanted.status !== "all" && !row.roles.includes(wanted.status)) {
        return false;
    }
    switch (wanted.bound) {
        case "disk25":
            return row.diskFree !== null && row.diskFree < 0.25;
        case "disk10":
            return row.diskFree !== null && row.diskFree < 0.1;
        case "mem85":
            return row.memoryUsed !== null && row.memoryUsed > 0.85;
        case "cpu80":
            return row.cpuBusy !== null && row.cpuBusy > 0.8;
        case "stale":
            return row.stale;
        default:
            return true;
    }
}

function compareRows(left: Row, right: Row): number {
    const value = (row: Row): string | number | null => {
        switch (wanted.sort) {
            case "name":
                return row.name;
            case "address":
                return row.address;
            case "roles":
                return row.roles.length;
            case "region":
                return row.region;
            case "leads":
                return row.leads;
            case "disk":
                return row.diskFree;
            case "memory":
                return row.memoryUsed;
            case "cpu":
                return row.cpuBusy;
            case "fresh":
                return row.stale ? Number.POSITIVE_INFINITY : row.observed;
        }
    };
    const a = value(left);
    const b = value(right);
    // Absence sorts last in both directions: a server that reported nothing
    // is not the smallest anything, it is the one thing this node cannot say.
    if (a === null && b === null) {
        return 0;
    }
    if (a === null) {
        return 1;
    }
    if (b === null) {
        return -1;
    }
    const by =
        typeof a === "number" && typeof b === "number"
            ? a - b
            : String(a).localeCompare(String(b));
    return by === 0 ? left.name.localeCompare(right.name) : by * wanted.dir;
}

function percentOf(fractionValue: number | null): string {
    return fractionValue === null ? "—" : `${Math.round(fractionValue * 100)}%`;
}

function freshCell(row: Row): string {
    if (row.observed === null) {
        return "—";
    }
    const said = row.observed === 0 ? "now" : `${row.observed}s ago`;
    return row.stale ? `stale · ${said}` : said;
}

function lamps(roles: readonly string[]): string {
    return ["serving", "writable", "coordinating"]
        .map((bit) => (roles.includes(bit) ? bit.charAt(0).toUpperCase() : "·"))
        .join("");
}

/** One pill group; the pressed pill carries `aria-pressed`. */
function pills(
    options: readonly { value: string; label: string }[],
    current: string,
    pick: (value: string) => void,
): HTMLElement {
    const group = made("div", "pills");
    group.setAttribute("role", "group");
    for (const option of options) {
        const pill = made("button", "pill");
        pill.type = "button";
        pill.textContent = option.label;
        pill.setAttribute("aria-pressed", String(option.value === current));
        pill.addEventListener("click", () => pick(option.value));
        group.appendChild(pill);
    }
    return group;
}

/**
 * Draw the filters and the table. Called on every read, like the map; the
 * filter and sort state survive a refresh because an operator mid-incident
 * did not choose those filters for one reading of the page.
 */
export function drawTable(
    filtersInto: HTMLElement,
    tableInto: HTMLElement,
    seen: Seen,
): void {
    const renderRows = (): void => {
        tableInto.replaceChildren();
        const rows = rowsOf(seen).filter(kept).sort(compareRows);
        const table = made("table", "servers");
        const head = table.createTHead();
        const headRow = head.insertRow();
        for (const column of COLUMNS) {
            const th = document.createElement("th");
            const button = made("button", "sort");
            button.type = "button";
            button.textContent =
                column.label +
                (wanted.sort === column.key ? (wanted.dir === 1 ? " ▲" : " ▼") : "");
            button.setAttribute(
                "aria-sort",
                wanted.sort === column.key
                    ? wanted.dir === 1
                        ? "ascending"
                        : "descending"
                    : "none",
            );
            button.addEventListener("click", () => {
                if (wanted.sort === column.key) {
                    wanted.dir = -wanted.dir as 1 | -1;
                } else {
                    wanted.sort = column.key;
                    wanted.dir = 1;
                }
                renderFilters();
                renderRows();
            });
            th.appendChild(button);
            headRow.appendChild(th);
        }
        const body = table.createTBody();
        for (const row of rows) {
            const tr = body.insertRow();
            tr.tabIndex = 0;
            tr.classList.add("server-row");
            const open = (): void => show(row.subject);
            tr.addEventListener("click", open);
            tr.addEventListener("keydown", (pressed) => {
                if (pressed.key === "Enter" || pressed.key === " ") {
                    pressed.preventDefault();
                    open();
                }
            });
            const cells: string[] = [
                row.name,
                row.address ?? "—",
                row.roles.length === 0 ? "drained" : lamps(row.roles),
                row.region ?? "—",
                row.leads ?? "—",
                percentOf(row.diskFree),
                percentOf(row.memoryUsed),
                percentOf(row.cpuBusy),
                freshCell(row),
            ];
            for (const text of cells) {
                tr.insertCell().textContent = text;
            }
        }
        tableInto.appendChild(table);
        if (rows.length === 0) {
            const none = made("p", "empty-note");
            none.textContent = "No server matches these filters.";
            tableInto.appendChild(none);
        }
    };

    const renderFilters = (): void => {
        filtersInto.replaceChildren();
        const find = made("input");
        find.type = "search";
        find.className = "find";
        find.placeholder = "find a server by name, address or id";
        find.value = wanted.find;
        find.setAttribute("aria-label", "find a server");
        // Typing re-renders the ROWS only: rebuilding the filter bar on every
        // keystroke would tear down the input the operator is typing into.
        find.addEventListener("input", () => {
            wanted.find = find.value.trim().toLowerCase();
            renderRows();
        });
        filtersInto.append(
            find,
            pills(STATUS_FILTERS, wanted.status, (value) => {
                wanted.status = value;
                renderFilters();
                renderRows();
            }),
            pills(BOUND_FILTERS, wanted.bound, (value) => {
                wanted.bound = value;
                renderFilters();
                renderRows();
            }),
        );
    };

    renderFilters();
    renderRows();
}
