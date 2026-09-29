import { describe, expect, test } from "bun:test";

import { AttachClient, attachUrl, CLOSE_ENDED, type AttachEvent, type SocketLike } from "./attach";

class FakeSocket implements SocketLike {
  readonly sent: string[] = [];
  closed = false;
  onopen: ((event: unknown) => void) | null = null;
  onclose: ((event: unknown) => void) | null = null;
  onerror: ((event: unknown) => void) | null = null;
  onmessage: ((event: { data: unknown }) => void) | null = null;

  send(data: string): void {
    this.sent.push(data);
  }

  close(): void {
    this.closed = true;
    this.onclose?.({});
  }

  /** The daemon closing the socket with a code (RFC 6455). */
  closeWith(code: number): void {
    this.closed = true;
    this.onclose?.({ code });
  }

  open(): void {
    this.onopen?.({});
  }

  deliver(data: unknown): void {
    this.onmessage?.({ data });
  }

  frames(): Record<string, unknown>[] {
    return this.sent.map((text) => JSON.parse(text) as Record<string, unknown>);
  }
}

function clientOn(
  dispatch: (call: string) => string,
): { client: AttachClient; sockets: FakeSocket[]; timers: (() => void)[]; events: AttachEvent[] } {
  const sockets: FakeSocket[] = [];
  const timers: (() => void)[] = [];
  const events: AttachEvent[] = [];
  const client = new AttachClient(
    { port: 8765, backoffMs: [10, 20], onEvent: (event) => events.push(event) },
    dispatch,
    () => {
      const socket = new FakeSocket();
      sockets.push(socket);
      return socket;
    },
    (fn) => timers.push(fn),
  );
  return { client, sockets, timers, events };
}

describe("the attach URL", () => {
  test("is always loopback, whatever the page was opened with", () => {
    expect(attachUrl(8765)).toBe("ws://127.0.0.1:8765/v1/attach");
  });

  test("refuses anything that is not a TCP port", () => {
    expect(() => attachUrl(0)).toThrow();
    expect(() => attachUrl(-1)).toThrow();
    expect(() => attachUrl(70_000)).toThrow();
    expect(() => attachUrl(1.5)).toThrow();
  });
});

describe("attaching", () => {
  test("announces itself with no credential in band: the upgrade carried it", () => {
    const { client, sockets } = clientOn(() => "{}");
    client.connect();
    expect(client.state).toBe("connecting");
    sockets[0]?.open();

    expect(client.state).toBe("attached");
    expect(sockets[0]?.frames()[0]).toEqual({
      hello: "passportsim-browser",
      label: "browser",
    });
  });

  test("learns the id the daemon minted and reports it", () => {
    const { client, sockets, events } = clientOn(() => "{}");
    client.connect();
    sockets[0]?.open();
    expect(client.instance).toBeNull();
    sockets[0]?.deliver(JSON.stringify({ attached: "b1" }));

    expect(client.instance).toBe("b1");
    expect(events).toEqual([{ kind: "attached", instance: "b1" }]);
    expect(sockets[0]?.frames()).toHaveLength(1);
  });

  test("asks for the clock lease and reports the daemon's answer", () => {
    const { client, sockets, events } = clientOn(() => "{}");
    client.connect();
    sockets[0]?.open();
    client.takeLease();
    sockets[0]?.deliver(JSON.stringify({ lease: "refused", holder: "agent" }));
    client.releaseLease();
    sockets[0]?.deliver(JSON.stringify({ lease: "released" }));

    expect(sockets[0]?.frames().slice(1)).toEqual([{ lease: "take" }, { lease: "release" }]);
    expect(events).toEqual([
      { kind: "lease", lease: "refused", holder: "agent" },
      { kind: "lease", lease: "released" },
    ]);
  });

  test("answers a core refusal with the core's ApiError JSON, not a message", () => {
    const body = JSON.stringify({ code: "E_STATE", number: 2, message: "no machine is booted" });
    const { client, sockets } = clientOn(() => {
      throw Object.assign(new Error(`pemu call failed with status 2: ${body}`), { body });
    });
    client.connect();
    sockets[0]?.open();
    sockets[0]?.deliver(JSON.stringify({ id: 9, call: "{}" }));

    expect(sockets[0]?.frames()[1]).toEqual({ id: 9, err: body });
  });

  test("answers a proxied command with what the registry returned", () => {
    const seen: string[] = [];
    const { client, sockets } = clientOn((call) => {
      seen.push(call);
      return '{"ok":true}';
    });
    client.connect();
    sockets[0]?.open();
    sockets[0]?.deliver(JSON.stringify({ id: 7, call: '{"cmd":"status"}' }));

    expect(seen).toEqual(['{"cmd":"status"}']);
    expect(sockets[0]?.frames()[1]).toEqual({ id: 7, ok: '{"ok":true}' });
  });

  test("reports a failed command as an error on the same request id", () => {
    const { client, sockets } = clientOn(() => {
      throw new Error("E_LEASE held by agent");
    });
    client.connect();
    sockets[0]?.open();
    sockets[0]?.deliver(JSON.stringify({ id: 3, call: "{}" }));

    expect(sockets[0]?.frames()[1]).toEqual({ id: 3, err: "E_LEASE held by agent" });
  });

  test("ignores a frame that is not a request rather than closing the socket", () => {
    const { client, sockets } = clientOn(() => "{}");
    client.connect();
    sockets[0]?.open();
    sockets[0]?.deliver(JSON.stringify({ note: "hello" }));
    sockets[0]?.deliver(new Uint8Array([1, 2]));

    expect(sockets[0]?.frames()).toHaveLength(1);
    expect(sockets[0]?.closed).toBe(false);
  });

  test("answers a malformed frame instead of throwing out of the callback", () => {
    const { client, sockets } = clientOn(() => "{}");
    client.connect();
    sockets[0]?.open();
    sockets[0]?.deliver("{not json");

    expect(sockets[0]?.frames()[1]).toHaveProperty("err");
  });
});

describe("reconnecting", () => {
  test("retries with the configured backoff after the daemon goes away", () => {
    const { client, sockets, timers } = clientOn(() => "{}");
    client.connect();
    sockets[0]?.open();
    sockets[0]?.close();

    expect(client.state).toBe("retrying");
    expect(timers).toHaveLength(1);
    timers[0]?.();
    expect(sockets).toHaveLength(2);
    sockets[1]?.open();
    expect(client.state).toBe("attached");
  });

  test("does not reconnect after the daemon ended the attach on purpose", () => {
    const { client, sockets, timers, events } = clientOn(() => "{}");
    client.connect();
    sockets[0]?.open();
    sockets[0]?.deliver(JSON.stringify({ attached: "b2" }));
    sockets[0]?.closeWith(CLOSE_ENDED);

    expect(client.state).toBe("closed");
    expect(client.instance).toBeNull();
    expect(timers).toHaveLength(0);
    expect(events.at(-1)).toEqual({ kind: "ended" });
  });

  test("stops for good once it is closed", () => {
    const { client, sockets, timers } = clientOn(() => "{}");
    client.connect();
    sockets[0]?.open();
    client.close();

    expect(client.state).toBe("closed");
    expect(timers).toHaveLength(0);
    client.connect();
    expect(sockets).toHaveLength(1);
  });
});
