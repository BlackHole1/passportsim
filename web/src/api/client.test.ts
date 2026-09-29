// Every call goes through one door and lands in the journal whatever the registry answered; the
// click-to-journal case is in `controls.test.ts`.

import { describe, expect, test } from "bun:test";
import { WasmCore, type CoreExports } from "../worker/core";
import { FakeCore } from "../worker/fakeCore";
import { CommandClient, workerTransport } from "./client";
import type { DoctorArgs } from "./commands";
import { CommandError, decodeResponse, encodeRequest } from "./envelope";
import { UiJournal } from "./journal";

function scripted(reply: { ok?: string; err?: string }): {
  transport: (request: string) => Promise<{ ok?: string; err?: string }>;
  requests: string[];
} {
  const requests: string[] = [];
  return {
    requests,
    transport: (request) => {
      requests.push(request);
      return Promise.resolve(reply);
    },
  };
}

describe("the request envelope", () => {
  test("it is the WebSocket body without the transport's id", () => {
    expect(JSON.parse(encodeRequest("input", { button: "ok", action: "click" }))).toEqual({
      cmd: "input",
      args: { button: "ok", action: "click" },
    });
  });

  test("a command with no arguments still sends an object, not null", () => {
    expect(JSON.parse(encodeRequest("status", undefined))).toEqual({ cmd: "status", args: {} });
  });

  test("an Output is unwrapped and a bare payload is accepted as the json", () => {
    expect(decodeResponse("status", { ok: '{"json":{"vt_us":5},"text":"vt 5"}' })).toEqual({
      json: { vt_us: 5 },
      text: "vt 5",
    });
    expect(decodeResponse("status", { ok: '{"ok":true}' })).toEqual({
      json: { ok: true },
      text: "",
    });
  });

  test("an ApiError body becomes a CommandError that keeps its code and retryability", () => {
    try {
      decodeResponse("run", {
        err: '{"code":"E_TIMEOUT","message":"no match","retryable":true,"serial_tail":["a"]}',
      });
      throw new Error("expected a refusal");
    } catch (error) {
      expect(error).toBeInstanceOf(CommandError);
      const failure = error as CommandError;
      expect(failure.command).toBe("run");
      expect(failure.body.code).toBe("E_TIMEOUT");
      expect(failure.retryable).toBe(true);
      expect(failure.body.serial_tail).toEqual(["a"]);
    }
  });

  test("a non-JSON answer is a refusal, not a SyntaxError the control has to catch", () => {
    expect(() => decodeResponse("status", { ok: "<html>gateway</html>" })).toThrow(CommandError);
    expect(() => decodeResponse("status", {})).toThrow(CommandError);
  });
});

describe("CommandClient", () => {
  test("every call is journaled before it is sent and settled when it answers", async () => {
    const journal = new UiJournal();
    const { transport, requests } = scripted({ ok: '{"ok":true}' });
    const client = new CommandClient(transport, { journal, nowPs: () => 42n });
    await client.call("input", { button: "ok", action: "click" });
    expect(requests).toHaveLength(1);
    const entry = journal.last();
    expect(entry?.command).toBe("input");
    expect(entry?.args).toEqual({ button: "ok", action: "click" });
    expect(entry?.vtPs).toBe(42n);
    expect(entry?.outcome.state).toBe("ok");
  });

  test("a refused call is still journaled, with the error body", async () => {
    const journal = new UiJournal();
    const { transport } = scripted({ err: '{"code":"E_LEASE","message":"agent holds the clock"}' });
    const client = new CommandClient(transport, { journal });
    await expect(client.call("input", { button: "up", action: "click" })).rejects.toBeInstanceOf(
      CommandError,
    );
    const entry = journal.last();
    expect(entry?.outcome.state).toBe("failed");
    expect(entry?.outcome.state === "failed" && entry.outcome.error.code).toBe("E_LEASE");
  });

  test("a transport that throws becomes an E_INTERNAL refusal rather than an unknown rejection", async () => {
    const journal = new UiJournal();
    const client = new CommandClient(() => Promise.reject(new Error("worker died")), { journal });
    await expect(client.call("status", {})).rejects.toThrow("worker died");
    const entry = journal.last();
    expect(entry?.outcome.state === "failed" && entry.outcome.error.code).toBe("E_INTERNAL");
  });

  test("tryCall swallows the refusal but keeps the journal entry", async () => {
    const journal = new UiJournal();
    const { transport } = scripted({ err: '{"code":"E_LEASE"}' });
    const client = new CommandClient(transport, { journal });
    expect(await client.tryCall("status", {})).toBeNull();
    expect(journal.length).toBe(1);
  });

  test("the generated DoctorArgs type is what `doctor` takes", async () => {
    const { transport, requests } = scripted({ ok: '{"json":{"ok":true},"text":"ok"}' });
    const client = new CommandClient(transport);
    const args: DoctorArgs = { report: { warnings: [] } };
    await client.call("doctor", args);
    expect(JSON.parse(requests[0] ?? "{}")).toEqual({ cmd: "doctor", args: { report: { warnings: [] } } });
  });
});

describe("workerTransport", () => {
  test("it pairs replies with requests by id and ignores an unknown id", async () => {
    const posted: { type: string; id: number; request: string }[] = [];
    const hub: { listener?: (event: { data: unknown }) => void } = {};
    const worker = {
      postMessage(message: { type: "call"; id: number; request: string }) {
        posted.push(message);
      },
      addEventListener(_type: "message", fn: (event: { data: never }) => void) {
        hub.listener = fn as (event: { data: unknown }) => void;
      },
    };
    const transport = workerTransport(worker, 1_000, () => 0);
    const first = transport(encodeRequest("status", {}));
    const second = transport(encodeRequest("ui", {}));
    expect(posted.map((m) => m.id)).toEqual([1, 2]);
    // A reply for an id nobody is waiting on (a previous machine's late answer) is dropped.
    hub.listener?.({ data: { type: "call", id: 99, ok: "{}" } });
    hub.listener?.({ data: { type: "call", id: 2, ok: '{"json":2}' } });
    hub.listener?.({ data: { type: "call", id: 1, ok: '{"json":1}' } });
    expect(await second).toEqual({ ok: '{"json":2}', err: undefined });
    expect(await first).toEqual({ ok: '{"json":1}', err: undefined });
  });

  test("an unanswered call times out as a retryable E_TIMEOUT", async () => {
    const timer: { fire?: () => void } = {};
    const transport = workerTransport(
      {
        postMessage() {},
        addEventListener() {},
      },
      5,
      (fn) => {
        timer.fire = fn;
      },
    );
    const pending = transport(encodeRequest("run", {}));
    timer.fire?.();
    const reply = await pending;
    expect(JSON.parse(reply.err ?? "{}")).toMatchObject({ code: "E_TIMEOUT", retryable: true });
  });
});

describe("against the fake core", () => {
  test("a call reaches pemu_call as the JSON body the registry will parse", async () => {
    const fake = new FakeCore();
    const core = WasmCore.build(fake as unknown as CoreExports, "{}");
    const client = new CommandClient((request) => Promise.resolve({ ok: core.call(request) }));
    await client.call("inspect", { what: ["tasks"] });
    expect(fake.calls).toHaveLength(1);
    expect(JSON.parse(fake.calls[0] ?? "{}")).toEqual({
      cmd: "inspect",
      args: { what: ["tasks"] },
    });
  });
});
