// Dirty-row bookkeeping. The core publishes one inclusive span of rows (`FRAME_DIRTY_FIRST` and
// `FRAME_DIRTY_LAST`, `DIRTY_NONE` for none), with no columns, so renderers upload whole rows.

/**
 * The union of the row spans waiting for upload. The published span is a delta and uploads are
 * throttled, so skipped slices must accumulate. One span, not a set: far-apart edits upload the
 * rows between too, which keeps this allocation-free.
 */
export class DirtySpan {
  private first = -1;
  private last = -1;

  constructor(private readonly rows: number) {}

  get pending(): boolean {
    return this.first >= 0;
  }

  get firstRow(): number {
    return this.first;
  }

  get lastRow(): number {
    return this.last;
  }

  /** Adds rows `[first, last]`, clamped to the panel; a span wholly outside or reversed adds nothing. */
  add(first: number, last: number): void {
    const from = Math.max(0, first);
    const to = Math.min(this.rows - 1, last);
    if (to < from) {
      return;
    }
    if (this.first < 0) {
      this.first = from;
      this.last = to;
      return;
    }
    this.first = Math.min(this.first, from);
    this.last = Math.max(this.last, to);
  }

  addAll(): void {
    this.add(0, this.rows - 1);
  }

  clear(): void {
    this.first = -1;
    this.last = -1;
  }
}

/**
 * Views of a framebuffer starting at each row, created once per backing buffer. `texSubImage2D`
 * takes a view and no offset, and a fresh `subarray` per upload would allocate. They alias wasm
 * memory, so they are rebuilt when the buffer identity or offset changes.
 */
export class RowViews {
  private buffer: ArrayBufferLike | null = null;
  private byteOffset = -1;
  private views: (Uint16Array | undefined)[];
  created = 0;

  constructor(
    private readonly width: number,
    private readonly height: number,
  ) {
    this.views = new Array<Uint16Array | undefined>(height);
  }

  /** A view of `pixels` from row `first` to the end. `first` must be in range. */
  from(pixels: Uint16Array, first: number): Uint16Array {
    if (pixels.buffer !== this.buffer || pixels.byteOffset !== this.byteOffset) {
      this.buffer = pixels.buffer;
      this.byteOffset = pixels.byteOffset;
      this.views.fill(undefined);
    }
    let view = this.views[first];
    if (view === undefined) {
      const rows = this.height - first;
      view = new Uint16Array(
        pixels.buffer,
        pixels.byteOffset + first * this.width * Uint16Array.BYTES_PER_ELEMENT,
        Math.max(0, Math.min(rows * this.width, pixels.length - first * this.width)),
      );
      this.views[first] = view;
      this.created += 1;
    }
    return view;
  }
}
