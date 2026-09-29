import { describe, expect, test } from "bun:test";

import {
  MAX_QUEUED_PACKETS,
  RELAY_HELLO,
  RelayClient,
  decodeBase64,
  readRelayWindow,
  relayRequest,
  relayUrl,
  type RelaySocketLike,
} from "./relay";

class FakeSocket implements RelaySocketLike {
  binaryType = "";
  readonly sentText: string[] = [];
  readonly sentBinary: Uint8Array[] = [];
  closed = false;
  onopen: ((event: unknown) => void) | null = null;
  onclose: ((event: unknown) => void) | null = null;
  onerror: ((event: unknown) => void) | null = null;
  onmessage: ((event: { data: unknown }) => void) | null = null;

  send(data: string | ArrayBufferLike | ArrayBufferView): void {
    if (typeof data === "string") {
      this.sentText.push(data);
    } else if (ArrayBuffer.isView(data)) {
      this.sentBinary.push(new Uint8Array(data.buffer.slice(data.byteOffset, data.byteOffset + data.byteLength)));
    } else {
      this.sentBinary.push(new Uint8Array(data));
    }
  }

  close(): void {
    this.closed = true;
  }

  open(): void {
    this.onopen?.({});
  }

  deliver(data: unknown): void {
    this.onmessage?.({ data });
  }
}

function clientOn(routes: { port: number; hostPort: number }[] = [{ port: 80, hostPort: 18080 }]): {
  client: RelayClient;
  sockets: FakeSocket[];
  errors: string[];
} {
  const sockets: FakeSocket[] = [];
  const errors: string[] = [];
  const client = new RelayClient({ port: 8765, routes, onError: (reason) => errors.push(reason) }, () => {
    const socket = new FakeSocket();
    sockets.push(socket);
    return socket;
  });
  return { client, sockets, errors };
}

describe("the @relay window", () => {
  test("the request carries the carrier's cursor, and a first look carries none", () => {
    expect(JSON.parse(relayRequest(null))).toEqual({ cmd: "@relay" });
    expect(JSON.parse(relayRequest("7"))).toEqual({ cmd: "@relay", args: { cursor: "7" } });
  });

  test("base64 packets decode to the bytes the core sent", () => {
    expect([...decodeBase64("")]).toEqual([]);
    expect([...decodeBase64("Zm9vYmFy")]).toEqual([...new TextEncoder().encode("foobar")]);
    expect([...decodeBase64("AQIAAAA=")]).toEqual([1, 2, 0, 0, 0]);
  });

  test("an answer reads into the bridge state, its allowlist and its packets", () => {
    const window = readRelayWindow(
      JSON.stringify({
        attached: true,
        routes: [{ port: 80, host_port: 18080 }],
        packets: ["Zm9v", "YmFy"],
        cursor: "12",
        dropped: "3",
      }),
    );
    expect(window.attached).toBe(true);
    expect(window.routes).toEqual([{ port: 80, hostPort: 18080 }]);
    expect(window.packets.map((p) => new TextDecoder().decode(p))).toEqual(["foo", "bar"]);
    expect(window.cursor).toBe("12");
    expect(window.dropped).toBe("3");
  });

  test("an answer of another shape throws rather than looking like a machine with no bridge", () => {
    for (const bad of [
      "{}",
      JSON.stringify({ attached: true, routes: [], packets: [], cursor: 12, dropped: "0" }),
      JSON.stringify({ attached: true, routes: [{ port: 80 }], packets: [], cursor: "0", dropped: "0" }),
      JSON.stringify({ attached: true, routes: [], packets: [7], cursor: "0", dropped: "0" }),
    ]) {
      expect(() => readRelayWindow(bad), bad).toThrow();
    }
  });
});

describe("the relay client", () => {
  test("the URL is always loopback and the port is checked", () => {
    expect(relayUrl(8765)).toBe("ws://127.0.0.1:8765/v1/relay");
    for (const bad of [0, -1, 65_536, 1.5]) {
      expect(() => relayUrl(bad)).toThrow();
    }
  });

  test("the hello announces the machine's allowlist and nothing else", () => {
    const { client, sockets } = clientOn([
      { port: 80, hostPort: 18080 },
      { port: 443, hostPort: 18443 },
    ]);
    client.connect();
    sockets[0]?.open();
    expect(sockets[0]?.binaryType).toBe("arraybuffer");
    expect(JSON.parse(sockets[0]?.sentText[0] ?? "null")).toEqual({
      hello: RELAY_HELLO,
      routes: [
        { port: 80, host_port: 18080 },
        { port: 443, host_port: 18443 },
      ],
    });
  });

  test("packets wait for `ready` and then go out in order, and answers come back as bytes", () => {
    const { client, sockets } = clientOn();
    client.connect();
    const socket = sockets[0];
    socket?.open();
    client.send([new Uint8Array([1, 2]), new Uint8Array([3])]);
    expect(socket?.sentBinary).toEqual([]);
    expect(client.ready).toBe(false);

    socket?.deliver(JSON.stringify({ ready: true, routes: [], buffer: 64 }));
    expect(client.ready).toBe(true);
    expect(socket?.sentBinary.map((p) => [...p])).toEqual([[1, 2], [3]]);
    client.send([new Uint8Array([4])]);
    expect(socket?.sentBinary.map((p) => [...p])).toEqual([[1, 2], [3], [4]]);

    socket?.deliver(new Uint8Array([9, 9]).buffer);
    socket?.deliver(new Uint8Array([8]));
    expect(client.drain().map((p) => [...p])).toEqual([[9, 9], [8]]);
    expect(client.drain()).toEqual([]);
  });

  test("a refused hello ends the relay with the daemon's reason and sends nothing more", () => {
    const { client, sockets, errors } = clientOn();
    client.connect();
    sockets[0]?.open();
    sockets[0]?.deliver(JSON.stringify({ error: "host port 8765 is one this process listens on" }));
    expect(client.state).toBe("refused");
    expect(errors[0]).toContain("host port 8765");
    client.send([new Uint8Array([1])]);
    expect(sockets[0]?.sentBinary).toEqual([]);
  });

  test("a socket that never becomes ready ends rather than holding a guest's packets for ever", () => {
    const { client, sockets, errors } = clientOn();
    client.connect();
    sockets[0]?.open();
    for (let i = 0; i <= MAX_QUEUED_PACKETS; i += 1) {
      client.send([new Uint8Array([i & 0xff])]);
    }
    expect(client.state).toBe("refused");
    expect(errors[0]).toContain(`${MAX_QUEUED_PACKETS} packets`);
    expect(sockets[0]?.closed).toBe(true);
  });

  test("a socket that closes ends the relay and is not reconnected", () => {
    const { client, sockets, errors } = clientOn();
    client.connect();
    sockets[0]?.open();
    sockets[0]?.deliver(JSON.stringify({ ready: true }));
    sockets[0]?.onclose?.({});
    expect(client.state).toBe("refused");
    expect(errors[0]).toContain("closed");
    expect(sockets).toHaveLength(1);
  });

  test("close is final and frees the socket's handlers", () => {
    const { client, sockets, errors } = clientOn();
    client.connect();
    const socket = sockets[0];
    socket?.open();
    client.close();
    expect(client.state).toBe("closed");
    expect(socket?.closed).toBe(true);
    expect(socket?.onmessage).toBeNull();
    socket?.onclose?.({});
    expect(errors).toEqual([]);
  });
});
