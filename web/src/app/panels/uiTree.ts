// The UI tree tab. `ui` builds, prunes and renders the tree, one node per line, two spaces of
// indent per level:
//
//   - <class> ["<text>"] [<x>,<y> <w>x<h>] [<state>...] [bg=#rrggbb] [border=#rrggbb] [value=..] e<N>
//
// This file reads those lines back, applies `ui --diff` answers and maps a row's box onto the
// glass. The tab reads only while shown, and then only when the screen changed, at most once per
// {@link MIN_READ_INTERVAL_MS}: a `ui` call may run the guest to the next LVGL safe point.

export interface GuestRect {
  readonly x: number;
  readonly y: number;
  readonly w: number;
  readonly h: number;
}

export interface UiRow {
  readonly depth: number;
  readonly cls: string | null;
  readonly text: string | null;
  readonly rect: GuestRect | null;
  readonly extras: string;
  /** `eN`, valid for one `ui_rev`. */
  readonly ref: string | null;
  readonly raw: string;
}

export interface UiAnswer {
  readonly ui_rev?: unknown;
  readonly text?: unknown;
  readonly diff?: unknown;
  readonly counts?: unknown;
  readonly screen?: unknown;
  readonly settled?: unknown;
  readonly truncated?: unknown;
}

export interface DiffEntry {
  readonly line: number;
  readonly text: string;
}

export const MIN_READ_INTERVAL_MS = 1_000;

export const DEFAULT_SCREEN = { w: 240, h: 320 } as const;

const NODE_LINE =
  /^- (\S+)(?: ("(?:[^"\\]|\\.)*"))? \[(-?\d+),(-?\d+) (-?\d+)x(-?\d+)\](.*?)(?: (e\d+))?$/;

/** Reads one line. A non-node line (a walker warning) is kept as a row, so the pane still shows it. */
export function parseLine(line: string): UiRow {
  const indent = line.length - line.trimStart().length;
  const body = line.slice(indent);
  const depth = Math.floor(indent / 2);
  const match = NODE_LINE.exec(body);
  if (match === null) {
    return { depth, cls: null, text: null, rect: null, extras: "", ref: null, raw: body };
  }
  const [, cls, quoted, x, y, w, h, extras, ref] = match;
  return {
    depth,
    cls: cls ?? null,
    text: quoted === undefined ? null : unquote(quoted),
    rect: { x: Number(x), y: Number(y), w: Number(w), h: Number(h) },
    extras: (extras ?? "").trim(),
    ref: ref ?? null,
    raw: body,
  };
}

/**
 * Undoes the common escapes of Rust's `{:?}` in a quoted label; an unknown escape is left as
 * printed.
 */
export function unquote(quoted: string): string {
  const inner = quoted.slice(1, -1);
  return inner.replace(/\\(u\{([0-9a-fA-F]+)\}|.)/g, (whole, esc: string, hex: string | undefined) => {
    if (hex !== undefined) {
      const code = Number.parseInt(hex, 16);
      return Number.isFinite(code) && code <= 0x10ffff ? String.fromCodePoint(code) : whole;
    }
    switch (esc) {
      case "n":
        return "\n";
      case "t":
        return "\t";
      case "r":
        return "\r";
      case "0":
        return "\0";
      case '"':
      case "\\":
      case "'":
        return esc;
      default:
        return whole;
    }
  });
}

export function parseTree(text: string): UiRow[] {
  return text === "" ? [] : text.split("\n").map(parseLine);
}

/**
 * Applies a `ui --diff <rev>` answer to the lines of revision `rev`. `ui` reports only changed
 * positions and nothing for removed lines (`pemu_api::commands::ui::diff_lines`), so the new
 * length comes from `counts.shown`; a diff past that length is refused.
 */
export function applyDiff(
  lines: readonly string[],
  diff: readonly DiffEntry[],
  shown: number,
): string[] | null {
  const next = lines.slice(0, shown);
  for (const entry of diff) {
    if (!Number.isSafeInteger(entry.line) || entry.line < 0 || entry.line >= shown) {
      return null;
    }
    while (next.length < entry.line) {
      next.push("");
    }
    next[entry.line] = entry.text;
  }
  if (next.length !== shown || next.some((line) => line === undefined)) {
    return null;
  }
  for (let index = lines.length; index < shown; index += 1) {
    if (!diff.some((entry) => entry.line === index)) {
      return null;
    }
  }
  return next;
}

export interface CssBox {
  readonly left: number;
  readonly top: number;
  readonly width: number;
  readonly height: number;
}

/**
 * The rectangle over the glass for a row's box, from guest pixels to the drawn CSS size, clipped
 * to the screen; `null` when nothing of it is on screen.
 */
export function highlightBox(
  rect: GuestRect,
  screen: { readonly w: number; readonly h: number },
  drawn: { readonly width: number; readonly height: number },
): CssBox | null {
  if (screen.w <= 0 || screen.h <= 0 || drawn.width <= 0 || drawn.height <= 0) {
    return null;
  }
  const x0 = Math.max(0, rect.x);
  const y0 = Math.max(0, rect.y);
  const x1 = Math.min(screen.w, rect.x + rect.w);
  const y1 = Math.min(screen.h, rect.y + rect.h);
  if (x1 <= x0 || y1 <= y0) {
    return null;
  }
  const sx = drawn.width / screen.w;
  const sy = drawn.height / screen.h;
  return { left: x0 * sx, top: y0 * sy, width: (x1 - x0) * sx, height: (y1 - y0) * sy };
}

export interface UiTreeState {
  readonly rev: number | null;
  readonly lines: readonly string[];
  readonly rows: readonly UiRow[];
  readonly screen: { readonly w: number; readonly h: number };
  readonly objects: number | null;
  /** Whether the read happened at an LVGL safe point; `null` when the build cannot tell. */
  readonly settled: boolean | null;
  readonly truncated: boolean;
  readonly via: "full" | "diff" | null;
}

export function readArgs(rev: number | null): { diff?: number } {
  return rev === null ? {} : { diff: rev };
}

function number(value: unknown): number | null {
  return typeof value === "number" && Number.isSafeInteger(value) ? value : null;
}

function record(value: unknown): Record<string, unknown> {
  return typeof value === "object" && value !== null ? (value as Record<string, unknown>) : {};
}

function diffEntries(value: unknown): DiffEntry[] | null {
  if (!Array.isArray(value)) {
    return null;
  }
  const out: DiffEntry[] = [];
  for (const entry of value) {
    const item = record(entry);
    const line = number(item.line);
    if (line === null || typeof item.text !== "string") {
      return null;
    }
    out.push({ line, text: item.text });
  }
  return out;
}

export const EMPTY: UiTreeState = {
  rev: null,
  lines: [],
  rows: [],
  screen: DEFAULT_SCREEN,
  objects: null,
  settled: null,
  truncated: false,
  via: null,
};

/**
 * The tab's state after one `ui` answer: a diff is applied to the held lines when it fits,
 * otherwise the answer's own `text` is taken whole.
 */
export function accept(state: UiTreeState, answer: UiAnswer, asked: number | null): UiTreeState {
  const rev = number(answer.ui_rev);
  const counts = record(answer.counts);
  const screenRecord = record(answer.screen);
  const w = number(screenRecord.w);
  const h = number(screenRecord.h);
  const screen = w !== null && h !== null && w > 0 && h > 0 ? { w, h } : state.screen;
  const text = typeof answer.text === "string" ? answer.text : null;
  const diff = diffEntries(answer.diff);
  const shown = number(counts.shown);
  let lines: string[] | null = null;
  let via: UiTreeState["via"] = "full";
  if (asked !== null && asked === state.rev && diff !== null && shown !== null) {
    lines = applyDiff(state.lines, diff, shown);
    via = "diff";
  }
  if (lines === null) {
    lines = text === null || text === "" ? [] : text.split("\n");
    via = "full";
  }
  return {
    rev,
    lines,
    rows: lines.map(parseLine),
    screen,
    objects: number(counts.objects),
    settled: typeof answer.settled === "boolean" ? answer.settled : null,
    truncated: answer.truncated === true,
    via,
  };
}

export function summaryLine(state: UiTreeState): string {
  if (state.rev === null) {
    return "not read yet";
  }
  const parts = [`ui_rev ${state.rev}`, `${state.rows.length} line(s)`];
  if (state.objects !== null) {
    parts.push(`${state.objects} LVGL object(s)`);
  }
  if (state.settled === false) {
    parts.push("not at a safe point; the tree may be mid-update");
  }
  if (state.truncated) {
    parts.push("cut at max_nodes");
  }
  return parts.join(" | ");
}

export type ReadOutcome =
  | { readonly ok: true; readonly json: UiAnswer }
  | { readonly ok: false; readonly code: string; readonly message: string };

export interface FollowerDeps {
  /** One `ui` call through the page's `CommandClient`, so every read is journaled. */
  readonly read: (args: { diff?: number; include_style: true }) => Promise<ReadOutcome>;
  readonly now: () => number;
  readonly schedule: (fn: () => void, ms: number) => void;
  readonly onState: (state: UiTreeState) => void;
  readonly onError: (message: string | null) => void;
}

/**
 * Keeps the tree current while, and only while, the tab is shown: one read on open, then on
 * {@link changed}, one at a time and {@link MIN_READ_INTERVAL_MS} apart. Later reads ask for a
 * diff; if another reader moved the revision (`E_USAGE`) it reads whole. Reads ask for
 * `include_style`, since the official menu shows its selection only through card colours.
 */
export class UiTreeFollower {
  private shown = false;
  private inFlight = false;
  private again = false;
  private armed = false;
  private lastRead = Number.NEGATIVE_INFINITY;
  private current: UiTreeState = EMPTY;

  constructor(
    private readonly deps: FollowerDeps,
    private readonly minIntervalMs: number = MIN_READ_INTERVAL_MS,
  ) {}

  get state(): UiTreeState {
    return this.current;
  }

  get visible(): boolean {
    return this.shown;
  }

  setVisible(visible: boolean): void {
    const opened = visible && !this.shown;
    this.shown = visible;
    if (opened) {
      this.request(true);
    }
  }

  changed(): void {
    this.request(false);
  }

  reset(): void {
    this.current = EMPTY;
    this.deps.onState(this.current);
    this.deps.onError(null);
    this.request(true);
  }

  private request(now: boolean): void {
    if (!this.shown) {
      return;
    }
    if (this.inFlight) {
      this.again = true;
      return;
    }
    const wait = now ? 0 : this.lastRead + this.minIntervalMs - this.deps.now();
    if (wait > 0) {
      if (!this.armed) {
        this.armed = true;
        this.deps.schedule(() => {
          this.armed = false;
          this.request(true);
        }, wait);
      }
      return;
    }
    void this.read();
  }

  private async read(): Promise<void> {
    this.inFlight = true;
    this.again = false;
    this.lastRead = this.deps.now();
    const asked = this.current.rev;
    try {
      const outcome = await this.deps.read({ ...readArgs(asked), include_style: true });
      if (outcome.ok) {
        this.current = accept(this.current, outcome.json, asked);
        this.deps.onState(this.current);
        this.deps.onError(null);
      } else if (asked !== null && outcome.code === "E_USAGE") {
        this.current = { ...this.current, rev: null };
        this.again = true;
        this.lastRead = Number.NEGATIVE_INFINITY;
      } else {
        this.deps.onError(`${outcome.code}: ${outcome.message}`);
      }
    } finally {
      this.inFlight = false;
    }
    if (this.again) {
      this.request(false);
    }
  }
}
