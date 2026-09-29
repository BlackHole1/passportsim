// Input stamping, the fixed-layout `pemu_input` encoder and the journal export. Inputs are stamped
// with the next slice boundary, so a paced session equals its journal replayed at `Max`; stamping
// at the current time would let the host's event delivery decide the instant.

import {
  AT_NOW,
  ButtonId,
  InputBatchHeader,
  INPUT_BATCH_MAGIC,
  InputKind,
  InputRecord,
} from "./layout";

export interface StampedInput {
  readonly atPs: bigint;
  readonly kind: InputKind;
  readonly a: number;
  readonly b: number;
  readonly c: number;
  readonly payload: Uint8Array | null;
}

export type JournalDoor = "input" | "registry";

export const JOURNAL_FORMAT = 1;

/**
 * The journal a paced session exports, replayable at `Max`. It is the machine's own record, holding
 * inputs the Worker stamped and those registry commands journaled, in `seq` order. Live microphone,
 * network and HCI chunks are dropped unless `includeSecrets`: their entries keep `atPs`, `seq` and
 * `door`, carry a `null` event and a `dropped` marker, and the export is `replayable: false`.
 */
export interface JournalExport {
  readonly format: number;
  readonly abiVersion: number;
  /** `false` when any entry was dropped, so a replay would not reach the session. */
  readonly replayable: boolean;
  readonly dropped: readonly { readonly kind: string; readonly seq: string }[];
  readonly entries: readonly JournalEntry[];
}

export interface JournalEntry {
  readonly atPs: string;
  readonly seq: string;
  readonly door: JournalDoor;
  /** The event in the serde form of `pemu_core::input::InputEvent`; `null` when dropped. */
  readonly event: unknown;
  readonly dropped?: { readonly kind: string; readonly seq: string; readonly len: number };
}

/** Reads the `@journal` answer; throws on another shape or format, so a bad export never looks empty. */
export function journalFromCore(answer: string, abiVersion: number): JournalExport {
  const parsed = JSON.parse(answer) as {
    format?: unknown;
    replayable?: unknown;
    dropped?: unknown;
    entries?: unknown;
  };
  if (parsed.format !== JOURNAL_FORMAT) {
    throw new Error(`\`@journal\` answered format ${String(parsed.format)}, not ${JOURNAL_FORMAT}`);
  }
  if (!Array.isArray(parsed.entries) || !Array.isArray(parsed.dropped) || typeof parsed.replayable !== "boolean") {
    throw new Error("`@journal` answered no `entries`, `dropped` and `replayable`");
  }
  return {
    format: JOURNAL_FORMAT,
    abiVersion,
    replayable: parsed.replayable,
    dropped: parsed.dropped.map((raw: unknown, index) => {
      const item = raw as { kind?: unknown; seq?: unknown };
      if (typeof item.kind !== "string" || typeof item.seq !== "string") {
        throw new Error(`\`@journal\` dropped item ${index} is not {kind, seq}`);
      }
      return { kind: item.kind, seq: item.seq };
    }),
    entries: parsed.entries.map((raw: unknown, index) => {
      const entry = raw as {
        at_ps?: unknown;
        seq?: unknown;
        door?: unknown;
        event?: unknown;
        dropped?: { kind?: unknown; seq?: unknown; len?: unknown };
      };
      const marker = entry.dropped;
      if (
        typeof entry.at_ps !== "string" ||
        typeof entry.seq !== "string" ||
        (entry.door !== "input" && entry.door !== "registry") ||
        entry.event === undefined ||
        (entry.event === null) !== (marker !== undefined) ||
        (marker !== undefined &&
          (typeof marker.kind !== "string" || typeof marker.seq !== "string" || typeof marker.len !== "number"))
      ) {
        throw new Error(`\`@journal\` entry ${index} is not {at_ps, seq, door, event, dropped?}`);
      }
      return {
        atPs: entry.at_ps,
        seq: entry.seq,
        door: entry.door,
        event: entry.event,
        ...(marker
          ? { dropped: { kind: marker.kind as string, seq: marker.seq as string, len: marker.len as number } }
          : {}),
      };
    }),
  };
}

export function encodeBatch(inputs: readonly StampedInput[]): Uint8Array {
  const blobLength = inputs.reduce((total, input) => total + (input.payload?.length ?? 0), 0);
  const blobOffset = InputBatchHeader.SIZE + inputs.length * InputRecord.SIZE;
  const buffer = new ArrayBuffer(blobOffset + blobLength);
  const view = new DataView(buffer);
  const bytes = new Uint8Array(buffer);

  view.setUint32(InputBatchHeader.MAGIC, INPUT_BATCH_MAGIC, true);
  view.setUint32(InputBatchHeader.COUNT, inputs.length, true);
  view.setUint32(InputBatchHeader.BLOB_OFF, blobOffset, true);
  view.setUint32(InputBatchHeader.BLOB_LEN, blobLength, true);

  let blobAt = blobOffset;
  inputs.forEach((input, index) => {
    const at = InputBatchHeader.SIZE + index * InputRecord.SIZE;
    view.setBigInt64(at + InputRecord.AT_PS, input.atPs, true);
    view.setUint32(at + InputRecord.KIND, input.kind, true);
    view.setUint32(at + InputRecord.A, input.a >>> 0, true);
    view.setUint32(at + InputRecord.B, input.b >>> 0, true);
    view.setUint32(at + InputRecord.C, input.c >>> 0, true);
    const payload = input.payload;
    if (payload && payload.length > 0) {
      view.setUint32(at + InputRecord.BLOB_OFF, blobAt, true);
      view.setUint32(at + InputRecord.BLOB_LEN, payload.length, true);
      bytes.set(payload, blobAt);
      blobAt += payload.length;
    } else {
      view.setUint32(at + InputRecord.BLOB_OFF, 0, true);
      view.setUint32(at + InputRecord.BLOB_LEN, 0, true);
    }
  });
  return bytes;
}

/**
 * Collects input between slices, stamps it with the next slice boundary and journals it. The
 * boundary comes from the pacing loop, the only part that knows where the next slice ends.
 */
export class InputStamper {
  private pending: StampedInput[] = [];
  private journal: StampedInput[] = [];

  push(kind: InputKind, a = 0, b = 0, c = 0, payload: Uint8Array | null = null): void {
    this.pending.push({ atPs: AT_NOW, kind, a, b, c, payload });
  }

  button(id: ButtonId, down: boolean): void {
    this.push(InputKind.Button, id, down ? 1 : 0);
  }

  power(down: boolean): void {
    this.push(InputKind.Power, 0, down ? 1 : 0);
  }

  serial(channel: number, data: Uint8Array): void {
    this.push(InputKind.SerialIn, channel, 0, 0, data);
  }

  micChunk(seq: bigint, samples: Int16Array): void {
    const payload = new Uint8Array(samples.buffer.slice(samples.byteOffset, samples.byteOffset + samples.byteLength));
    this.push(InputKind.MicChunk, Number(seq & 0xffff_ffffn), Number((seq >> 32n) & 0xffff_ffffn), 0, payload);
  }

  get hasPending(): boolean {
    return this.pending.length > 0;
  }

  /** Stamps everything queued with `boundaryPs`; `null` when nothing was queued. */
  drain(boundaryPs: bigint): StampedInput[] | null {
    if (this.pending.length === 0) {
      return null;
    }
    const stamped = this.pending.map((input) => ({ ...input, atPs: boundaryPs }));
    this.pending = [];
    this.journal.push(...stamped);
    return stamped;
  }

  /** What this Worker stamped; the exported journal (`journalFromCore`) also holds registry inputs. */
  get entries(): readonly StampedInput[] {
    return this.journal;
  }
}
