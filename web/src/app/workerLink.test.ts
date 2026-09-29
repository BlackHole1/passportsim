import { describe, expect, test } from "bun:test";
import { workerTransport } from "../api/client";
import { createInputCell } from "../worker/pacing";
import { linkWorker, type WorkerLike } from "./workerLink";

function fakeWorker() {
  const log: string[] = [];
  const listeners: ((event: { data: unknown }) => void)[] = [];
  const worker: WorkerLike = {
    postMessage(message) {
      log.push(`post ${(message as { type: string }).type}`);
    },
    addEventListener(_type, listener) {
      listeners.push(listener);
    },
  };
  return {
    worker,
    log,
    emit: (data: unknown) => {
      for (const listener of listeners) listener({ data });
    },
  };
}

describe("linkWorker", () => {
  test("without inputSab a post is never followed by a notify", () => {
    const fake = fakeWorker();
    const link = linkWorker(fake.worker, () => fake.log.push("notify"));
    link.postMessage({ type: "boot" });
    // A `ready` from a page that is not isolated carries no cell.
    fake.emit({ type: "ready", abiVersion: 3 });
    link.postMessage({ type: "button" });
    link.postMessage({ type: "mode" });
    expect(fake.log).toEqual(["post boot", "post button", "post mode"]);
    expect(link.inputSab).toBeNull();
  });

  test("once ready carried inputSab, every post is followed by a notify on that cell", () => {
    const fake = fakeWorker();
    const cell = createInputCell();
    const notified: SharedArrayBuffer[] = [];
    const link = linkWorker(fake.worker, (shared) => {
      notified.push(shared);
      fake.log.push("notify");
    });
    link.postMessage({ type: "boot" });
    fake.emit({ type: "ready", abiVersion: 3, inputSab: cell });
    link.postMessage({ type: "button" });
    link.postMessage({ type: "mode" });
    expect(fake.log).toEqual(["post boot", "post button", "notify", "post mode", "notify"]);
    expect(notified.every((shared) => shared === cell)).toBe(true);
    expect(link.inputSab).toBe(cell);
  });

  test("registry calls through the transport notify too, and the real notify bumps the counter", async () => {
    const fake = fakeWorker();
    const cell = createInputCell();
    const link = linkWorker(fake.worker);
    fake.emit({ type: "ready", abiVersion: 3, inputSab: cell });
    const transport = workerTransport(link, 1_000, () => undefined);
    const answer = transport('{"cmd":"status"}');
    expect(fake.log).toEqual(["post call"]);
    expect(Atomics.load(new Int32Array(cell), 0)).toBe(1);
    fake.emit({ type: "call", id: 1, ok: "{}" });
    expect(await answer).toEqual({ ok: "{}", err: undefined });
  });

  test("a reboot whose ready has no cell stops the notifies", () => {
    const fake = fakeWorker();
    const link = linkWorker(fake.worker, () => fake.log.push("notify"));
    fake.emit({ type: "ready", abiVersion: 3, inputSab: createInputCell() });
    link.postMessage({ type: "button" });
    fake.emit({ type: "ready", abiVersion: 3 });
    link.postMessage({ type: "button" });
    expect(fake.log).toEqual(["post button", "notify", "post button"]);
  });
});
