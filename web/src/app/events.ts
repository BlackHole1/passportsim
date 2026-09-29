// The Events tab: the machine's `events` ring and the page's own journal (where inputs live)
// interleaved in one list. A host event's `arg` is printed as a number only; its meaning depends
// on the kind, and guessing would be a claim nothing here supports.

import { formatVirtualTime } from "./header";
import { EventKind } from "../worker/layout";

export interface HostEventIn {
  readonly kind: number;
  readonly vtPs: bigint;
  readonly arg: bigint;
}

const KIND_NAMES: ReadonlyMap<number, string> = new Map(
  Object.entries(EventKind).map(([name, value]) => [value as number, name]),
);

/**
 * The name of a host event kind, or `kind N` for one this build does not know: a newer core can
 * emit one, and dropping it would make the tab lie.
 */
export function eventName(kind: number): string {
  return KIND_NAMES.get(kind) ?? `kind ${kind}`;
}

export type EventSource = "machine" | "ui";

export interface EventRow {
  readonly seq: number;
  readonly source: EventSource;
  readonly name: string;
  readonly vt: string;
  readonly detail: string;
  /** The journal entry a `ui` row shows, for its copy actions; `null` for a machine row. */
  readonly journalSeq: number | null;
}

/** How many rows the tab keeps; older ones are dropped and counted. */
export const EVENT_LIMIT = 1_000;

/**
 * Rows in arrival order, not sorted by virtual time: a UI call is stamped with the time the page
 * last heard, up to one slice behind, so sorting would put an input before what it caused.
 */
export class EventLog {
  private readonly rows: EventRow[] = [];
  private nextSeq = 1;
  private droppedRows = 0;

  constructor(private readonly limit: number = EVENT_LIMIT) {}

  list(): readonly EventRow[] {
    return this.rows;
  }

  get dropped(): number {
    return this.droppedRows;
  }

  pushHost(events: readonly HostEventIn[]): readonly EventRow[] {
    return events.map((event) =>
      this.push({
        source: "machine",
        name: eventName(event.kind),
        vt: formatVirtualTime(event.vtPs),
        detail: `arg ${event.arg}`,
        journalSeq: null,
      }),
    );
  }

  pushCall(command: string, args: unknown, vtPs: bigint | null, journalSeq: number | null = null): EventRow {
    return this.push({
      source: "ui",
      name: command,
      vt: vtPs === null ? "--" : formatVirtualTime(vtPs),
      detail: summarize(args),
      journalSeq,
    });
  }

  private push(row: Omit<EventRow, "seq">): EventRow {
    const full: EventRow = { seq: this.nextSeq++, ...row };
    this.rows.push(full);
    while (this.rows.length > this.limit) {
      this.rows.shift();
      this.droppedRows += 1;
    }
    return full;
  }
}

/** Command arguments as one line, capped so a long NDEF write cannot push the column off-screen. */
export function summarize(args: unknown, limit = 80): string {
  if (args === undefined || args === null) {
    return "";
  }
  const text = typeof args === "string" ? args : JSON.stringify(args);
  if (text === undefined) {
    return "";
  }
  return text.length <= limit ? text : `${text.slice(0, limit - 3)}...`;
}
