// The daemon attach client, the page end of `crates/pemu-host/src/attach.rs`. A browser-hosted
// machine attaches to the local daemon as `b1`, `b2` over `ws://127.0.0.1:<port>/v1/attach`, and
// agent commands are proxied into the Worker through the same registry the CLI uses.
//
// The socket is authorized at its upgrade by the page's session cookie and the daemon's `Host` and
// `Origin` checks; no token travels inside it. The URL is always loopback: a page that attached to
// a remote "daemon" would hand an agent channel to whoever answered.
//
// Frames (`attach.rs` is the reference): `hello`, then `{attached: "b1"}`, then `{id, call}`
// answered with `{id, ok}` or `{id, err}` (the core's ApiError JSON). `{lease: "take"}` and
// `{lease: "release"}` ask for the clock lease.

export interface SocketLike {
  send(data: string): void;
  close(): void;
  onopen: ((event: unknown) => void) | null;
  onclose: ((event: { code?: number } | unknown) => void) | null;
  onerror: ((event: unknown) => void) | null;
  onmessage: ((event: { data: unknown }) => void) | null;
}

export type SocketFactory = (url: string) => SocketLike;

export interface AttachRequest {
  readonly id: number;
  readonly call: string;
}

export type AttachReply =
  | { readonly id: number; readonly ok: string }
  | { readonly id: number; readonly err: string };

export type AttachState = "idle" | "connecting" | "attached" | "retrying" | "closed";

export type AttachEvent =
  | { readonly kind: "attached"; readonly instance: string }
  | { readonly kind: "lease"; readonly lease: string; readonly holder?: string }
  /** The daemon ended the attach on purpose (`stop`, shutdown); the client does not reconnect. */
  | { readonly kind: "ended" };

export interface AttachOptions {
  readonly port: number;
  readonly label?: string;
  /** Reconnect backoff in milliseconds, doubled up to the last entry. */
  readonly backoffMs?: readonly number[];
  readonly onEvent?: (event: AttachEvent) => void;
}

const DEFAULT_BACKOFF_MS = [250, 500, 1000, 2000, 5000] as const;

export const CLOSE_ENDED = 4000;

/** The loopback attach URL for `port`; built here so a crafted query cannot point it elsewhere. */
export function attachUrl(port: number): string {
  if (!Number.isInteger(port) || port <= 0 || port > 65535) {
    throw new Error(`${port} is not a TCP port`);
  }
  return `ws://127.0.0.1:${port}/v1/attach`;
}

export class AttachClient {
  private socket: SocketLike | null = null;
  private attempt = 0;
  private closed = false;
  private currentState: AttachState = "idle";
  private minted: string | null = null;

  constructor(
    private readonly options: AttachOptions,
    private readonly dispatch: (call: string) => string,
    private readonly openSocket: SocketFactory = (url) => new WebSocket(url) as unknown as SocketLike,
    private readonly schedule: (fn: () => void, ms: number) => void = (fn, ms) => {
      setTimeout(fn, ms);
    },
  ) {}

  get state(): AttachState {
    return this.currentState;
  }

  get instance(): string | null {
    return this.minted;
  }

  connect(): void {
    if (this.closed) {
      return;
    }
    this.currentState = "connecting";
    const socket = this.openSocket(attachUrl(this.options.port));
    this.socket = socket;
    socket.onopen = () => {
      this.attempt = 0;
      this.currentState = "attached";
      socket.send(JSON.stringify({ hello: "passportsim-browser", label: this.options.label ?? "browser" }));
    };
    socket.onmessage = (event) => {
      this.handle(event.data);
    };
    socket.onerror = () => {
      socket.close();
    };
    socket.onclose = (event) => {
      this.socket = null;
      this.minted = null;
      const code = (event as { code?: unknown } | null)?.code;
      if (code === CLOSE_ENDED && !this.closed) {
        // Ended on purpose: attaching again would mint a new id for a machine an agent just stopped.
        this.closed = true;
        this.options.onEvent?.({ kind: "ended" });
      }
      if (this.closed) {
        this.currentState = "closed";
        return;
      }
      this.currentState = "retrying";
      this.schedule(() => this.connect(), this.backoff());
    };
  }

  takeLease(): void {
    this.socket?.send(JSON.stringify({ lease: "take" }));
  }

  releaseLease(): void {
    this.socket?.send(JSON.stringify({ lease: "release" }));
  }

  close(): void {
    this.closed = true;
    this.currentState = "closed";
    this.socket?.close();
    this.socket = null;
  }

  /** Handles one frame. Malformed frames are answered, never thrown out of the socket callback. */
  private handle(data: unknown): void {
    if (typeof data !== "string") {
      return;
    }
    let frame: Record<string, unknown>;
    try {
      frame = JSON.parse(data) as Record<string, unknown>;
    } catch (error) {
      this.reply({ id: 0, err: describe(error) });
      return;
    }
    if (typeof frame.attached === "string") {
      this.minted = frame.attached;
      this.options.onEvent?.({ kind: "attached", instance: frame.attached });
      return;
    }
    if (typeof frame.lease === "string") {
      this.options.onEvent?.({
        kind: "lease",
        lease: frame.lease,
        ...(typeof frame.holder === "string" ? { holder: frame.holder } : {}),
      });
      return;
    }
    if (typeof frame.id !== "number" || typeof frame.call !== "string") {
      return;
    }
    const id = frame.id;
    try {
      this.reply({ id, ok: this.dispatch(frame.call) });
    } catch (error) {
      this.reply({ id, err: errorBody(error) });
    }
  }

  private reply(message: AttachReply): void {
    this.socket?.send(JSON.stringify(message));
  }

  private backoff(): number {
    const steps = this.options.backoffMs ?? DEFAULT_BACKOFF_MS;
    const at = Math.min(this.attempt, steps.length - 1);
    this.attempt += 1;
    return steps[at] ?? steps[steps.length - 1] ?? 1000;
  }
}

/** A failed dispatch's answer: the core's ApiError JSON when the error carries one, else the message. */
function errorBody(error: unknown): string {
  const body = (error as { body?: unknown } | null)?.body;
  return typeof body === "string" ? body : describe(error);
}

function describe(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}
