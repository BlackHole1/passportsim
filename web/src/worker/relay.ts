// The page's carrier of the Wi-Fi bridge, the browser end of `crates/pemu-host/src/relay_wisp.rs`.
// The virtual LAN turns a guest connection to an allowlisted port into WISP v1 client packets in a
// window the machine owns; this file moves them over `ws://127.0.0.1:<port>/v1/relay` to the
// daemon's WISP server and journals what comes back. It decides nothing about the protocol: the
// journaled `NetFrame`s already mark the run `live`, and it replays without any peer.
//
// The URL is always loopback, authorized at the upgrade by the daemon's usual `Host`, `Origin` and
// cookie checks; no token travels inside the socket. The allowlist is the machine's own, from the
// journaled `net_http --op bridge`, so a page cannot reach a port the instance was not bridged to.
//
// Frames (`relay_wisp.rs` is the reference): text `{hello, routes}` from the client, text
// `{ready, routes, buffer}` or `{error}` back, then one WISP packet per binary frame.

export const RELAY_HELLO = "passportsim-relay";

/** The reserved request that reads the machine's outbound window (`pemu_wasm::instance`). */
export const RELAY_REQUEST = "@relay";

export const MAX_QUEUED_PACKETS = 256;

export interface RelayRoute {
  readonly port: number;
  readonly hostPort: number;
}

export interface RelayWindow {
  readonly attached: boolean;
  readonly routes: readonly RelayRoute[];
  readonly packets: readonly Uint8Array[];
  readonly cursor: string;
  /** Client packets the window dropped before any carrier took them, as a decimal string. */
  readonly dropped: string;
}

export function relayRequest(cursor: string | null): string {
  return cursor === null
    ? JSON.stringify({ cmd: RELAY_REQUEST })
    : JSON.stringify({ cmd: RELAY_REQUEST, args: { cursor } });
}

/** The bytes of one base64 packet (RFC 4648 section 4). */
export function decodeBase64(text: string): Uint8Array {
  const binary = atob(text);
  const out = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i += 1) {
    out[i] = binary.charCodeAt(i);
  }
  return out;
}

/** Reads an `@relay` answer; throws on any other shape, so an unaware core fails loudly. */
export function readRelayWindow(answer: string): RelayWindow {
  const parsed = JSON.parse(answer) as {
    attached?: unknown;
    routes?: unknown;
    packets?: unknown;
    cursor?: unknown;
    dropped?: unknown;
  };
  if (
    typeof parsed.attached !== "boolean" ||
    !Array.isArray(parsed.routes) ||
    !Array.isArray(parsed.packets) ||
    typeof parsed.cursor !== "string" ||
    typeof parsed.dropped !== "string"
  ) {
    throw new Error("`@relay` answered no {attached, routes, packets, cursor, dropped}");
  }
  return {
    attached: parsed.attached,
    routes: parsed.routes.map((raw: unknown, index) => {
      const route = raw as { port?: unknown; host_port?: unknown };
      if (typeof route.port !== "number" || typeof route.host_port !== "number") {
        throw new Error(`\`@relay\` route ${index} is not {port, host_port}`);
      }
      return { port: route.port, hostPort: route.host_port };
    }),
    packets: parsed.packets.map((raw: unknown, index) => {
      if (typeof raw !== "string") {
        throw new Error(`\`@relay\` packet ${index} is not base64 text`);
      }
      return decodeBase64(raw);
    }),
    cursor: parsed.cursor,
    dropped: parsed.dropped,
  };
}

export interface RelaySocketLike {
  binaryType: string;
  send(data: string | ArrayBufferLike | ArrayBufferView): void;
  close(): void;
  onopen: ((event: unknown) => void) | null;
  onclose: ((event: unknown) => void) | null;
  onerror: ((event: unknown) => void) | null;
  onmessage: ((event: { data: unknown }) => void) | null;
}

export type RelaySocketFactory = (url: string) => RelaySocketLike;

export type RelayState = "connecting" | "ready" | "closed" | "refused";

export interface RelayOptions {
  readonly port: number;
  /** The machine's own allowlist, which this client announces and never adds to. */
  readonly routes: readonly RelayRoute[];
  readonly onError?: (reason: string) => void;
}

/** The loopback relay URL for `port`; built here so nothing on the page can point it elsewhere. */
export function relayUrl(port: number): string {
  if (!Number.isInteger(port) || port <= 0 || port > 65535) {
    throw new Error(`${port} is not a TCP port`);
  }
  return `ws://127.0.0.1:${port}/v1/relay`;
}

/**
 * One socket to the daemon's WISP server. It does not reconnect: a dropped socket took its streams
 * with it, and a silent new one would hand the guest a server that forgot its connections.
 */
export class RelayClient {
  private socket: RelaySocketLike | null = null;
  private current: RelayState = "connecting";
  private queued: Uint8Array[] = [];
  private inbound: Uint8Array[] = [];

  constructor(
    private readonly options: RelayOptions,
    private readonly openSocket: RelaySocketFactory = (url) =>
      new WebSocket(url) as unknown as RelaySocketLike,
  ) {}

  get state(): RelayState {
    return this.current;
  }

  get ready(): boolean {
    return this.current === "ready";
  }

  connect(): void {
    const socket = this.openSocket(relayUrl(this.options.port));
    socket.binaryType = "arraybuffer";
    this.socket = socket;
    socket.onopen = () => {
      socket.send(
        JSON.stringify({
          hello: RELAY_HELLO,
          routes: this.options.routes.map((route) => ({ port: route.port, host_port: route.hostPort })),
        }),
      );
    };
    socket.onmessage = (event) => this.handle(event.data);
    socket.onerror = () => this.fail("the relay socket failed");
    socket.onclose = () => {
      if (this.current !== "refused") {
        this.fail("the relay socket closed");
      }
    };
  }

  /**
   * Queues client packets, sending once ready. A queue past {@link MAX_QUEUED_PACKETS} ends the
   * relay rather than holding packets for a socket that will never be ready.
   */
  send(packets: readonly Uint8Array[]): void {
    if (this.current === "closed" || this.current === "refused") {
      return;
    }
    if (this.ready && this.socket) {
      for (const packet of packets) {
        this.socket.send(packet);
      }
      return;
    }
    this.queued.push(...packets);
    if (this.queued.length > MAX_QUEUED_PACKETS) {
      this.fail(`the relay was not ready after ${MAX_QUEUED_PACKETS} packets`);
    }
  }

  drain(): Uint8Array[] {
    const got = this.inbound;
    this.inbound = [];
    return got;
  }

  close(): void {
    this.current = "closed";
    this.queued = [];
    const socket = this.socket;
    this.socket = null;
    if (socket) {
      socket.onopen = null;
      socket.onclose = null;
      socket.onerror = null;
      socket.onmessage = null;
      socket.close();
    }
  }

  private handle(data: unknown): void {
    if (typeof data === "string") {
      this.handleText(data);
      return;
    }
    const bytes =
      data instanceof ArrayBuffer
        ? new Uint8Array(data)
        : ArrayBuffer.isView(data)
          ? new Uint8Array(data.buffer.slice(data.byteOffset, data.byteOffset + data.byteLength))
          : null;
    if (bytes !== null) {
      this.inbound.push(bytes);
    }
  }

  private handleText(text: string): void {
    let frame: { ready?: unknown; error?: unknown };
    try {
      frame = JSON.parse(text) as { ready?: unknown; error?: unknown };
    } catch {
      this.fail(`the relay answered ${text.slice(0, 200)}`);
      return;
    }
    if (typeof frame.error === "string") {
      this.current = "refused";
      this.options.onError?.(`the daemon refused the relay: ${frame.error}`);
      this.queued = [];
      return;
    }
    if (frame.ready !== true) {
      this.fail("the relay answered no `ready`");
      return;
    }
    this.current = "ready";
    const queued = this.queued;
    this.queued = [];
    this.send(queued);
  }

  private fail(reason: string): void {
    if (this.current === "closed" || this.current === "refused") {
      return;
    }
    this.current = "refused";
    this.queued = [];
    this.options.onError?.(reason);
    const socket = this.socket;
    this.socket = null;
    socket?.close();
  }
}
