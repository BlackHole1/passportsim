// The Fidelity tab renders what `inspect fidelity` returns and applies the rules each class
// implies; the table itself stays in the registry. A: validated against device traces. B: enough
// for unmodified ESP-IDF drivers, timing approximate. C: behavioural; assertions are annotated as
// emulator-only evidence. U: unmodelled; a run that touched one cannot PASS under `--strict`.

export type FidelityClass = "A" | "B" | "C" | "U";

export interface FidelityRow {
  readonly subsystem: string;
  readonly class: FidelityClass;
  readonly mayAssert?: string;
  readonly needsDevice?: string;
}

export function isFidelityClass(value: unknown): value is FidelityClass {
  return value === "A" || value === "B" || value === "C" || value === "U";
}

/** Reads the rows of an `inspect fidelity` result, skipping unknown shapes rather than guessing. */
export function parseRows(json: unknown): readonly FidelityRow[] {
  const source = extractRows(json);
  const rows: FidelityRow[] = [];
  for (const entry of source) {
    if (typeof entry !== "object" || entry === null) {
      continue;
    }
    const record = entry as Record<string, unknown>;
    const subsystem = record.subsystem ?? record.name;
    const klass = record.class ?? record.fidelity;
    if (typeof subsystem !== "string" || !isFidelityClass(klass)) {
      continue;
    }
    rows.push({
      subsystem,
      class: klass,
      ...(typeof record.may_assert === "string" ? { mayAssert: record.may_assert } : {}),
      ...(typeof record.needs_device === "string" ? { needsDevice: record.needs_device } : {}),
    });
  }
  return rows;
}

function extractRows(json: unknown): readonly unknown[] {
  if (Array.isArray(json)) {
    return json;
  }
  if (typeof json === "object" && json !== null) {
    const record = json as Record<string, unknown>;
    for (const key of ["subsystems", "rows", "fidelity"]) {
      const value = record[key];
      if (Array.isArray(value)) {
        return value;
      }
    }
  }
  return [];
}

export function needsAnnotation(klass: FidelityClass): boolean {
  return klass === "C" || klass === "U";
}

export function annotation(klass: FidelityClass): string {
  switch (klass) {
    case "A":
      return "validated against device traces for the paths the official BSP uses";
    case "B":
      return "sufficient for unmodified ESP-IDF drivers; timing approximate, not device-validated";
    case "C":
      return "behavioural or host-bridged; an assertion here is emulator-only evidence";
    case "U":
      return "unmodelled; reads return reset values and a touch fails a strict run";
  }
}

export interface StrictVerdict {
  readonly canPass: boolean;
  readonly blocking: readonly string[];
  readonly annotated: readonly string[];
}

/**
 * The verdict for the classes a run touched (the receipt's `classes_touched`): a U touch blocks
 * PASS under `--strict` (exit 7), a C touch is a caveat (exit 10).
 */
export function strictVerdict(touched: Readonly<Record<string, readonly string[]>>): StrictVerdict {
  const blocking = [...(touched.U ?? [])];
  const annotated = [...(touched.C ?? [])];
  return { canPass: blocking.length === 0, blocking, annotated };
}

export function receiptLine(rows: readonly FidelityRow[]): string {
  if (rows.length === 0) {
    return "fidelity: unknown";
  }
  return `fidelity: ${rows.map((row) => `${row.subsystem} ${row.class}`).join(", ")}`;
}

export function byRisk(rows: readonly FidelityRow[]): readonly FidelityRow[] {
  const rank: Record<FidelityClass, number> = { U: 0, C: 1, B: 2, A: 3 };
  return [...rows].sort(
    (a, b) => rank[a.class] - rank[b.class] || a.subsystem.localeCompare(b.subsystem),
  );
}
