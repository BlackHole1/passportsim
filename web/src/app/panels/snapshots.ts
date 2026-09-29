// The Snapshots card and rewind ring: 20 in-memory points, one every 2 virtual seconds. The cadence
// is virtual so a slow or `Max`-paced machine keeps the same history. Each point stores the panel
// frame, because a restored machine repaints only when the guest next draws, which a paused UI may
// never do.

export const REWIND_SLOTS = 20;

/** One every 2 virtual seconds, in picoseconds. */
export const REWIND_INTERVAL_PS = 2_000_000_000_000n;

/** The per-snapshot size target, which the card warns about exceeding. */
export const SNAPSHOT_SIZE_TARGET_BYTES = 1_000_000;

export interface RewindPoint {
  readonly seq: number;
  readonly vtPs: bigint;
  readonly bytes: Uint8Array;
  /** The panel as it was, in the renderer's RGB565 layout; `null` before the first paint. */
  readonly frame: Uint16Array | null;
}

export function pointBytes(point: RewindPoint): number {
  return point.bytes.byteLength + (point.frame?.byteLength ?? 0);
}

export interface RewindStats {
  readonly count: number;
  readonly totalBytes: number;
  readonly overTarget: number;
  readonly spanPs: bigint;
}

export interface RewindSource {
  snapshot(): Uint8Array;
  frame(): Uint16Array | null;
}

/**
 * The rewind ring: a fixed array reused in place, since it is the page's only structure holding
 * tens of megabytes and per-point allocation would keep old points alive until a GC.
 */
export class RewindRing {
  private readonly points: RewindPoint[] = [];
  /**
   * The registry's snapshot name per `seq`, since `snapshot restore` takes the name `save` was given.
   * A point with no name cannot be restored.
   */
  private readonly registryIds = new Map<number, string>();
  private nextSeq = 1;
  private lastPs: bigint | null = null;

  constructor(
    private readonly slots: number = REWIND_SLOTS,
    private readonly intervalPs: bigint = REWIND_INTERVAL_PS,
  ) {}

  list(): readonly RewindPoint[] {
    return this.points;
  }

  latest(): RewindPoint | null {
    return this.points[this.points.length - 1] ?? null;
  }

  get(seq: number): RewindPoint | null {
    return this.points.find((point) => point.seq === seq) ?? null;
  }

  /**
   * Takes a point if the interval has passed; the first call always does. Virtual time that went
   * backwards (a restore) re-anchors instead, or every scrub would evict the history being scrubbed.
   */
  maybeCapture(nowPs: bigint, source: RewindSource): RewindPoint | null {
    if (this.lastPs !== null && nowPs < this.lastPs) {
      this.lastPs = nowPs;
      return null;
    }
    if (this.lastPs !== null && nowPs - this.lastPs < this.intervalPs) {
      return null;
    }
    this.lastPs = nowPs;
    return this.capture(nowPs, source);
  }

  /** Takes a point now, whatever the interval says; the Snap button in the header. */
  capture(nowPs: bigint, source: RewindSource): RewindPoint {
    const frame = source.frame();
    const point: RewindPoint = {
      seq: this.nextSeq++,
      vtPs: nowPs,
      bytes: source.snapshot(),
      // Copied: the view is over wasm memory, which the next slice overwrites.
      frame: frame === null ? null : new Uint16Array(frame),
    };
    this.points.push(point);
    while (this.points.length > this.slots) {
      const evicted = this.points.shift();
      if (evicted) {
        this.registryIds.delete(evicted.seq);
      }
    }
    return point;
  }

  setRegistryId(seq: number, id: string): void {
    this.registryIds.set(seq, id);
  }

  registryId(seq: number): string | null {
    return this.registryIds.get(seq) ?? null;
  }

  clear(): void {
    this.points.length = 0;
    this.registryIds.clear();
    this.lastPs = null;
  }

  stats(): RewindStats {
    const first = this.points[0];
    const last = this.points[this.points.length - 1];
    return {
      count: this.points.length,
      totalBytes: this.points.reduce((sum, point) => sum + pointBytes(point), 0),
      overTarget: this.points.filter((point) => pointBytes(point) > SNAPSHOT_SIZE_TARGET_BYTES)
        .length,
      spanPs: first && last ? last.vtPs - first.vtPs : 0n,
    };
  }
}

export interface Rewind {
  readonly point: RewindPoint;
  readonly frame: Uint16Array | null;
}

/**
 * Picks the point the scrubber landed on. Its positions are points, not a time axis: after
 * eviction the points are unevenly spaced, and a time axis would offer positions with no point.
 */
export function scrubTo(ring: RewindRing, seq: number): Rewind | null {
  const point = ring.get(seq);
  return point === null ? null : { point, frame: point.frame };
}

/**
 * The name a `snapshot save` result carries, or `null`. Only `name` is read; guessing one would
 * restore the wrong point.
 */
export function snapshotIdFrom(json: unknown): string | null {
  if (typeof json !== "object" || json === null || Array.isArray(json)) {
    return null;
  }
  const name = (json as Record<string, unknown>).name;
  return typeof name === "string" && name.length > 0 ? name : null;
}

export function pointLabel(point: RewindPoint): string {
  const ms = point.vtPs / 1_000_000_000n;
  return `${ms / 1000n}.${(ms % 1000n).toString().padStart(3, "0")} s`;
}

/**
 * The name the card saves a point under. `snapshot` accepts only `^[A-Za-z0-9_.-]{1,64}$`, so the
 * spaced label cannot be it; `seq` makes it unique and the instant makes it findable in `snapshot list`.
 */
export function pointName(point: RewindPoint): string {
  return `rewind-${point.seq}-${pointLabel(point).replace(" ", "")}`;
}
