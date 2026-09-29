// The Inspect tab: a section picker and a renderer generic over the JSON `inspect` returns, so a
// field the command gains never shows as an empty column. The picker offers only the sections the
// command has. It never sends `nvs_values` and never un-redacts, so a screenshot of the tab holds
// no more than the command's own output.

import type { InspectArgs } from "../../api/commands";

export type InspectReport = InspectArgs["what"][number];

export const INSPECT_REPORTS = [
  { what: "tasks", label: "Tasks", summary: "FreeRTOS tasks and stack high-water marks" },
  { what: "heap", label: "Heap", summary: "free, minimum and largest block per region" },
  { what: "nvs", label: "NVS", summary: "namespaces and keys, values redacted" },
  { what: "lvgl", label: "LVGL", summary: "the object tree the UI is drawn from" },
  { what: "fidelity", label: "Fidelity", summary: "the per-subsystem class table" },
] as const satisfies readonly {
  readonly what: InspectReport;
  readonly label: string;
  readonly summary: string;
}[];

export class InspectFormError extends Error {
  constructor(
    readonly field: string,
    message: string,
  ) {
    super(message);
    this.name = "InspectFormError";
  }
}

/** The `inspect` arguments for a picker state; a repeated section is collapsed, not refused. */
export function toArgs(what: readonly InspectReport[]): InspectArgs {
  const picked: InspectReport[] = [];
  for (const one of what) {
    if (!INSPECT_REPORTS.some((report) => report.what === one)) {
      throw new InspectFormError("what", `${one} is not an inspect section`);
    }
    if (!picked.includes(one)) {
      picked.push(one);
    }
  }
  if (picked.length === 0) {
    throw new InspectFormError("what", "pick at least one section");
  }
  return { what: picked };
}

export function fromArgs(args: InspectArgs): { what: readonly InspectReport[] } {
  return { what: args.what };
}

export interface ReportRow {
  readonly columns: readonly string[];
}

export interface RenderedReport {
  readonly headers: readonly string[];
  readonly rows: readonly ReportRow[];
  readonly text: string | null;
}

/**
 * Turns a result into a table: an array of objects becomes rows with the union of their keys as
 * headers, in first-seen order. Anything else shows the command's own `text`.
 */
export function renderReport(json: unknown, text: string): RenderedReport {
  if (!Array.isArray(json) || json.length === 0) {
    return { headers: [], rows: [], text };
  }
  const headers: string[] = [];
  for (const entry of json) {
    if (typeof entry !== "object" || entry === null || Array.isArray(entry)) {
      return { headers: [], rows: [], text };
    }
    for (const key of Object.keys(entry as Record<string, unknown>)) {
      if (!headers.includes(key)) {
        headers.push(key);
      }
    }
  }
  const rows = json.map((entry) => ({
    columns: headers.map((key) => cell((entry as Record<string, unknown>)[key])),
  }));
  return { headers, rows, text: null };
}

export function cell(value: unknown): string {
  if (value === null || value === undefined) {
    return "";
  }
  if (typeof value === "object") {
    return JSON.stringify(value);
  }
  return String(value);
}
