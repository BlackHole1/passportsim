// The UI journal: every registry call the page made. `copy.ts` renders an entry and `recorder.ts`
// turns a session into scenario YAML. An entry keeps the command, its argument object (the argv is
// built per shell from it) and its virtual time (the recorder derives waits from it). The session's
// secret set outlives the ring.

import type { CommandName } from "./commands";
import type { ApiErrorBody, Json } from "./envelope";
import { SecretValues, addCallSecrets } from "./redact";

export type JournalOutcome =
  | { readonly state: "pending" }
  | {
      readonly state: "ok";
      readonly text: string;
      /**
       * Virtual time the call itself advanced (`elapsed_vt_us`), or `null`. The recorder subtracts it
       * from the gap to the next call, since the replayed step advances it again.
       */
      readonly elapsedVtUs: number | null;
    }
  | { readonly state: "failed"; readonly error: ApiErrorBody };

export interface JournalRecord {
  /** Monotonic within one session, and the identity the UI list keys on. */
  readonly seq: number;
  readonly command: CommandName;
  readonly args: Json;
  /** Virtual time when the control fired, in picoseconds, or `null` before a machine exists. */
  readonly vtPs: bigint | null;
  readonly outcome: JournalOutcome;
}

export interface JournalEntry {
  readonly seq: number;
  settle(result: { ok: true; text: string; elapsedVtUs?: number | null } | { ok: false; error: ApiErrorBody }): void;
}

export const JOURNAL_LIMIT = 2_000;

/**
 * The session's log. Entries are appended when a call is made, not answered, so the order is the
 * user's even when calls overlap. Past {@link JOURNAL_LIMIT} the oldest drop and are counted, so the
 * recorder can say an export is incomplete.
 */
export class UiJournal {
  private readonly entries: JournalRecord[] = [];
  private nextSeq = 1;
  private dropped = 0;
  private readonly listeners = new Set<(record: JournalRecord) => void>();

  /**
   * Every by-value secret any call of this session carried; never dropped, so an entry that fell off
   * the ring still masks its value elsewhere.
   */
  readonly secrets = new SecretValues();

  constructor(private readonly limit: number = JOURNAL_LIMIT) {}

  list(): readonly JournalRecord[] {
    return this.entries;
  }

  get droppedCount(): number {
    return this.dropped;
  }

  get length(): number {
    return this.entries.length;
  }

  last(): JournalRecord | null {
    return this.entries[this.entries.length - 1] ?? null;
  }

  subscribe(listener: (record: JournalRecord) => void): () => void {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  }

  record(command: CommandName, args: Json, vtPs: bigint | null): JournalEntry {
    const seq = this.nextSeq++;
    addCallSecrets(this.secrets, command, args);
    const record: JournalRecord = {
      seq,
      command,
      args,
      vtPs,
      outcome: { state: "pending" },
    };
    this.entries.push(record);
    while (this.entries.length > this.limit) {
      this.entries.shift();
      this.dropped += 1;
    }
    this.emit(record);
    return {
      seq,
      settle: (result) => {
        this.settle(seq, result);
      },
    };
  }

  private settle(
    seq: number,
    result: { ok: true; text: string; elapsedVtUs?: number | null } | { ok: false; error: ApiErrorBody },
  ): void {
    const at = this.entries.findIndex((entry) => entry.seq === seq);
    if (at < 0) {
      return;
    }
    const previous = this.entries[at];
    if (!previous) {
      return;
    }
    const outcome: JournalOutcome = result.ok
      ? { state: "ok", text: result.text, elapsedVtUs: result.elapsedVtUs ?? null }
      : { state: "failed", error: result.error };
    const updated: JournalRecord = { ...previous, outcome };
    this.entries[at] = updated;
    this.emit(updated);
  }

  private emit(record: JournalRecord): void {
    for (const listener of this.listeners) {
      listener(record);
    }
  }
}
