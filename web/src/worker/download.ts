// Byte counts of the files the page may download: the core and the bundled firmware of a boot, and
// a play's firmware (`app/play.ts`), so the page can say what it is waiting for. Counted as the
// bytes arrive; nothing here estimates.

/**
 * Which file: the `pemu_wasm` core, the bundled firmware a demo boot fetches by its `fw` name, or
 * the firmware of a play the page fetches from the play site.
 */
export type DownloadWhat = "core" | "firmware" | "play";

export interface DownloadProgress {
  readonly what: DownloadWhat;
  readonly received: number;
  /** The body's size from `Content-Length`; `null` when it is absent or counts encoded bytes. */
  readonly total: number | null;
  readonly done: boolean;
  /** Why the download failed; the loader throws the same error after reporting it. */
  readonly error?: string;
}

export type DownloadListener = (progress: DownloadProgress) => void;

/** Host milliseconds between two reports of one download. The first, the last and a failure are always reported. */
export const DOWNLOAD_REPORT_MS = 100;

/**
 * The decoded body's size, or `null`. A compressed response's `Content-Length` counts the encoded
 * bytes while the stream yields decoded ones, so it is not a total the counter can reach.
 */
export function contentLength(response: Pick<Response, "headers">): number | null {
  const encoding = response.headers.get("content-encoding")?.trim().toLowerCase() ?? "";
  if (encoding !== "" && encoding !== "identity") {
    return null;
  }
  const raw = response.headers.get("content-length")?.trim() ?? "";
  if (!/^\d+$/.test(raw)) {
    return null;
  }
  const length = Number(raw);
  return Number.isSafeInteger(length) ? length : null;
}

/**
 * Reports one download's progress to `listener`, at most once per {@link DOWNLOAD_REPORT_MS}
 * besides the first report, the final one and a failure. A count past the stated total drops the
 * total, since the header did not describe this body.
 */
export class DownloadReporter {
  private received = 0;
  private total: number | null = null;
  private lastMs = Number.NEGATIVE_INFINITY;
  private ended = false;

  constructor(
    private readonly what: DownloadWhat,
    private readonly listener: DownloadListener,
    private readonly nowMs: () => number,
  ) {}

  get bytes(): number {
    return this.received;
  }

  /** Whether the last byte arrived or the download failed; a later failure is not the download's. */
  get finished(): boolean {
    return this.ended;
  }

  /** Before the request, so the page knows what it waits for while no header has come back. */
  start(): void {
    this.report(false, true);
  }

  /** The response's headers arrived. */
  sized(total: number | null): void {
    this.total = total;
    this.report(false, true);
  }

  add(bytes: number): void {
    this.received += bytes;
    if (this.total !== null && this.received > this.total) {
      this.total = null;
    }
    this.report(false, false);
  }

  finish(): void {
    if (this.ended) {
      return;
    }
    this.ended = true;
    this.report(true, true);
  }

  fail(error: unknown): void {
    if (this.ended) {
      return;
    }
    this.ended = true;
    this.listener({
      what: this.what,
      received: this.received,
      total: this.total,
      done: true,
      error: error instanceof Error ? error.message : String(error),
    });
  }

  private report(done: boolean, always: boolean): void {
    const now = this.nowMs();
    if (!always && now - this.lastMs < DOWNLOAD_REPORT_MS) {
      return;
    }
    this.lastMs = now;
    this.listener({ what: this.what, received: this.received, total: this.total, done });
  }
}

/** Reads a whole body chunk by chunk, counting each into `reporter`. */
export async function readCounted(response: Response, reporter: DownloadReporter): Promise<Uint8Array> {
  reporter.sized(contentLength(response));
  if (response.body === null) {
    const bytes = new Uint8Array(await response.arrayBuffer());
    reporter.add(bytes.length);
    reporter.finish();
    return bytes;
  }
  const reader = response.body.getReader();
  const chunks: Uint8Array[] = [];
  for (;;) {
    const { done, value } = await reader.read();
    if (done) {
      break;
    }
    chunks.push(value);
    reporter.add(value.length);
  }
  // Joined once at the end rather than into a buffer of the header's size, which may be wrong.
  const bytes = new Uint8Array(reporter.bytes);
  let at = 0;
  for (const chunk of chunks) {
    bytes.set(chunk, at);
    at += chunk.length;
  }
  reporter.finish();
  return bytes;
}

/**
 * The same response with its body counted into `reporter` as it is consumed, for a consumer that
 * reads the stream itself (`WebAssembly.instantiateStreaming` compiles while the bytes arrive). The
 * headers are kept, so the `application/wasm` type still holds. A constructed response has no URL,
 * which costs the engine's cache of compiled code across visits.
 */
export function countedResponse(response: Response, reporter: DownloadReporter): Response {
  reporter.sized(contentLength(response));
  if (response.body === null) {
    reporter.finish();
    return response;
  }
  const counter = new TransformStream<Uint8Array, Uint8Array>({
    transform(chunk, controller) {
      reporter.add(chunk.length);
      controller.enqueue(chunk);
    },
    flush() {
      reporter.finish();
    },
  });
  return new Response(response.body.pipeThrough(counter), {
    status: response.status,
    statusText: response.statusText,
    headers: response.headers,
  });
}
