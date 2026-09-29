// The console model: raw bytes from the Worker become lines, ANSI stripped, capped at 400
// characters, repeats collapsed as `x12`. Lines are split from the bytes, and line marks only
// stamp virtual time, so a line whose mark was evicted is not lost. Evicted bytes become a
// visible gap line, so a burst that outran the ring never reads as seamless.

import type { SerialChannel } from "../api/commands";
import { RingId, SerialStream } from "../worker/layout";

export interface ConsoleLine {
  readonly stream: SerialChannel;
  readonly text: string;
  /** Absolute byte cursor of this line's newline. */
  readonly cursor: bigint;
  readonly vtPs: bigint | null;
  readonly repeat: number;
  readonly gap: boolean;
}

export interface SerialSliceIn {
  readonly stream: number;
  readonly bytes: Uint8Array;
  readonly dropped: bigint;
  readonly lines: readonly { readonly offset: bigint; readonly vtPs: bigint }[];
  readonly linesDropped: bigint;
}

export const MAX_LINE_CHARS = 400;

export const CONSOLE_SCROLLBACK = 5_000;

export function channelOf(ringId: number): SerialChannel {
  return ringId === RingId.Uart0Tx ? "uart0" : "usj";
}

export function streamOf(channel: SerialChannel): number {
  return channel === "uart0" ? SerialStream.Uart0Tx : SerialStream.UsjTx;
}

/**
 * Strips CSI and two-character ANSI escapes. OSC is left as text: its terminator differs between
 * engines, and guessing wrong would eat the rest of the line.
 */
export function stripAnsi(text: string): string {
  return text.replace(ANSI, "");
}

const ANSI = /\u001b\[[0-9;?]*[ -/]*[@-~]|\u001b[@-Z\\-_]/g;

/** One decoder for the whole console; one per line shows up in a boot burst. */
const DECODER = new TextDecoder();

export function presentLine(raw: string): string {
  const clean = stripAnsi(raw).replace(/\r$/, "");
  return clean.length <= MAX_LINE_CHARS ? clean : `${clean.slice(0, MAX_LINE_CHARS)}...`;
}

export interface LineFilter {
  readonly source: string;
  matches(line: ConsoleLine): boolean;
}

/**
 * `/re/flags` is a regular expression and anything else a case-insensitive substring, including
 * an unclosed `/pk_a` still being typed. An invalid pattern falls back to a substring search of
 * its body rather than throwing mid-keystroke.
 */
export function compileFilter(source: string): LineFilter | null {
  const trimmed = source.trim();
  if (trimmed.length === 0) {
    return null;
  }
  const asRegex = /^\/(.*)\/([gimsuy]*)$/.exec(trimmed);
  let needle = trimmed;
  if (asRegex) {
    const [, pattern = "", flags = ""] = asRegex;
    try {
      // `g` is dropped: a global regex carries `lastIndex` between `test` calls, so every other line
      // would fail to match.
      const re = new RegExp(pattern, flags.replace(/g/g, ""));
      return { source: trimmed, matches: (line) => re.test(line.text) };
    } catch {
      needle = pattern;
    }
  }
  const lowered = needle.toLowerCase();
  return {
    source: trimmed,
    matches: (line) => line.text.toLowerCase().includes(lowered),
  };
}

/** The serial line-state indicators, which reach the machine over RFC 2217. */
export interface LineState {
  readonly dtr: boolean;
  readonly rts: boolean;
}

interface StreamState {
  start: bigint;
  pending: number[];
}

/** The console's lines. Both streams share one list, so it shows which of ROM and app printed first. */
export class ConsoleModel {
  private readonly lines: ConsoleLine[] = [];
  private readonly streams = new Map<number, StreamState>();
  private filter: LineFilter | null = null;
  private capturing = false;
  private captured: string[] = [];
  private lineState: LineState = { dtr: false, rts: false };
  private droppedRows = 0;
  private readonly channels = new Set<SerialChannel>();

  constructor(private readonly scrollback: number = CONSOLE_SCROLLBACK) {}

  all(): readonly ConsoleLine[] {
    return this.lines;
  }

  streamsSeen(): readonly SerialChannel[] {
    return [...this.channels];
  }

  /**
   * The USB console (what `idf.py monitor` shows) once it has printed, else every stream. The ROM
   * prints its banner on both UART0 and USB, so a merged view shows it twice.
   */
  primary(): readonly ConsoleLine[] {
    const lines = this.visible();
    return this.channels.has("usj") ? lines.filter((line) => line.stream === "usj") : lines;
  }

  visible(): readonly ConsoleLine[] {
    const filter = this.filter;
    if (!filter) {
      return this.lines;
    }
    // A gap row always shows, so a filtered view never claims a continuity the stream lacks.
    return this.lines.filter((line) => line.gap || filter.matches(line));
  }

  get scrolledOut(): number {
    return this.droppedRows;
  }

  get filterSource(): string {
    return this.filter?.source ?? "";
  }

  setFilter(source: string): void {
    this.filter = compileFilter(source);
  }

  get captureMode(): boolean {
    return this.capturing;
  }

  /** Capture mode: everything printed while it is on is kept verbatim. */
  setCapture(on: boolean): void {
    this.capturing = on;
    if (!on) {
      return;
    }
    this.captured = [];
  }

  captureText(): string {
    return this.captured.join("\n");
  }

  get state(): LineState {
    return this.lineState;
  }

  setLineState(state: LineState): void {
    this.lineState = state;
  }

  /** Drops every line; on a reboot the byte cursors restart at zero. */
  clear(): void {
    this.lines.length = 0;
    this.channels.clear();
    this.streams.clear();
    this.captured = [];
    this.droppedRows = 0;
  }

  push(slice: SerialSliceIn): readonly ConsoleLine[] {
    const channel = channelOf(slice.stream);
    const state = this.streams.get(slice.stream) ?? { start: 0n, pending: [] };
    this.streams.set(slice.stream, state);
    const produced: ConsoleLine[] = [];

    if (slice.dropped > 0n) {
      // The evicted bytes include the rest of the partial line, so it is abandoned too.
      const resumeAt = state.start + BigInt(state.pending.length) + slice.dropped;
      state.pending = [];
      state.start = resumeAt;
      produced.push(
        this.append({
          stream: channel,
          text: `--- ${slice.dropped} bytes lost: the ${channel} ring overran ---`,
          cursor: resumeAt,
          vtPs: null,
          repeat: 1,
          gap: true,
        }),
      );
    }

    const stamps = new Map<bigint, bigint>();
    for (const mark of slice.lines) {
      stamps.set(mark.offset, mark.vtPs);
    }

    for (const byte of slice.bytes) {
      if (byte !== 0x0a) {
        state.pending.push(byte);
        continue;
      }
      const cursor = state.start + BigInt(state.pending.length);
      const text = presentLine(DECODER.decode(Uint8Array.from(state.pending)));
      state.start = cursor + 1n;
      state.pending = [];
      produced.push(
        this.append({
          stream: channel,
          text,
          cursor,
          vtPs: stamps.get(cursor) ?? null,
          repeat: 1,
          gap: false,
        }),
      );
    }
    return produced;
  }

  /**
   * Appends a line, collapsing an immediate identical repeat on the same stream. A gap row never
   * collapses: a count spanning a hole would claim the hole held the same line.
   */
  private append(line: ConsoleLine): ConsoleLine {
    if (!line.gap) {
      this.channels.add(line.stream);
    }
    if (this.capturing) {
      this.captured.push(line.text);
    }
    const previous = this.lines[this.lines.length - 1];
    if (
      previous &&
      !line.gap &&
      !previous.gap &&
      previous.stream === line.stream &&
      previous.text === line.text
    ) {
      const merged: ConsoleLine = {
        ...previous,
        cursor: line.cursor,
        vtPs: line.vtPs ?? previous.vtPs,
        repeat: previous.repeat + 1,
      };
      this.lines[this.lines.length - 1] = merged;
      return merged;
    }
    this.lines.push(line);
    while (this.lines.length > this.scrollback) {
      this.lines.shift();
      this.droppedRows += 1;
    }
    return line;
  }
}

export function lineStamp(line: ConsoleLine): string {
  if (line.vtPs === null) {
    return `@${line.cursor}`;
  }
  const ms = line.vtPs / 1_000_000_000n;
  return `${ms / 1000n}.${(ms % 1000n).toString().padStart(3, "0")}`;
}
