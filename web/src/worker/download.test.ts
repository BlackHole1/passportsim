import { describe, expect, test } from "bun:test";
import {
  contentLength,
  countedResponse,
  DOWNLOAD_REPORT_MS,
  DownloadReporter,
  readCounted,
  type DownloadProgress,
} from "./download";

function streamOf(chunks: readonly Uint8Array[]): ReadableStream<Uint8Array> {
  let next = 0;
  return new ReadableStream<Uint8Array>({
    pull(controller) {
      const chunk = chunks[next];
      next += 1;
      if (chunk === undefined) {
        controller.close();
      } else {
        controller.enqueue(chunk);
      }
    },
  });
}

function recorder(clock: { ms: number }) {
  const seen: DownloadProgress[] = [];
  return { seen, reporter: (what: "core" | "firmware") => new DownloadReporter(what, (p) => seen.push(p), () => clock.ms) };
}

describe("contentLength", () => {
  test("reads a plain length", () => {
    expect(contentLength(new Response(null, { headers: { "content-length": "24700000" } }))).toBe(24_700_000);
    expect(contentLength(new Response(null, { headers: { "content-length": " 12 " } }))).toBe(12);
  });

  test("is unknown when absent, malformed, or counting encoded bytes", () => {
    expect(contentLength(new Response(null))).toBeNull();
    expect(contentLength(new Response(null, { headers: { "content-length": "12, 12" } }))).toBeNull();
    expect(contentLength(new Response(null, { headers: { "content-length": "-1" } }))).toBeNull();
    expect(contentLength(new Response(null, { headers: { "content-length": "900", "content-encoding": "br" } }))).toBeNull();
    expect(contentLength(new Response(null, { headers: { "content-length": "900", "content-encoding": "identity" } }))).toBe(900);
  });
});

describe("DownloadReporter", () => {
  test("reports the start, then at most once per period, and always the end", () => {
    const clock = { ms: 0 };
    const { seen, reporter } = recorder(clock);
    const r = reporter("firmware");
    r.start();
    r.sized(1000);
    for (let i = 0; i < 10; i += 1) {
      clock.ms += DOWNLOAD_REPORT_MS / 4;
      r.add(100);
    }
    r.finish();
    expect(seen[0]).toEqual({ what: "firmware", received: 0, total: null, done: false });
    expect(seen[1]).toEqual({ what: "firmware", received: 0, total: 1000, done: false });
    // 10 chunks over 2.5 periods: two throttled reports between the header and the end.
    expect(seen.slice(2, -1).map((p) => p.received)).toEqual([400, 800]);
    expect(seen.at(-1)).toEqual({ what: "firmware", received: 1000, total: 1000, done: true });
  });

  test("drops a total the body runs past, since the header did not describe it", () => {
    const clock = { ms: 0 };
    const { seen, reporter } = recorder(clock);
    const r = reporter("core");
    r.sized(10);
    r.add(11);
    r.finish();
    expect(seen.at(-1)).toEqual({ what: "core", received: 11, total: null, done: true });
  });

  test("a failure is reported once, and not after the last byte arrived", () => {
    const clock = { ms: 0 };
    const { seen, reporter } = recorder(clock);
    const failing = reporter("firmware");
    failing.sized(100);
    failing.add(40);
    failing.fail(new Error("network error"));
    failing.fail(new Error("again"));
    expect(seen.filter((p) => p.error !== undefined)).toEqual([
      { what: "firmware", received: 40, total: 100, done: true, error: "network error" },
    ]);

    const done = reporter("core");
    done.finish();
    const before = seen.length;
    done.fail(new Error("CompileError"));
    expect(seen.length).toBe(before);
    expect(done.finished).toBe(true);
  });
});

describe("readCounted", () => {
  test("joins the chunks and counts each against the stated total", async () => {
    const clock = { ms: 0 };
    const { seen, reporter } = recorder(clock);
    const body = streamOf([new Uint8Array([1, 2, 3]), new Uint8Array([4, 5])]);
    const bytes = await readCounted(new Response(body, { headers: { "content-length": "5" } }), reporter("firmware"));
    expect([...bytes]).toEqual([1, 2, 3, 4, 5]);
    expect(seen[0]).toEqual({ what: "firmware", received: 0, total: 5, done: false });
    expect(seen.at(-1)).toEqual({ what: "firmware", received: 5, total: 5, done: true });
  });

  test("without a length it counts the bytes alone", async () => {
    const clock = { ms: 0 };
    const { seen, reporter } = recorder(clock);
    const bytes = await readCounted(new Response(streamOf([new Uint8Array(7)])), reporter("firmware"));
    expect(bytes.length).toBe(7);
    expect(seen.at(-1)).toEqual({ what: "firmware", received: 7, total: null, done: true });
  });
});

describe("countedResponse", () => {
  test("hands on the same bytes and headers, counting them as the consumer reads", async () => {
    const clock = { ms: 0 };
    const { seen, reporter } = recorder(clock);
    const original = new Response(streamOf([new Uint8Array([0, 0x61]), new Uint8Array([0x73, 0x6d])]), {
      headers: { "content-type": "application/wasm", "content-length": "4" },
    });
    const counted = countedResponse(original, reporter("core"));
    expect(counted.headers.get("content-type")).toBe("application/wasm");
    expect(counted.status).toBe(200);
    // Nothing is read until the consumer reads.
    expect(seen).toEqual([{ what: "core", received: 0, total: 4, done: false }]);
    expect([...new Uint8Array(await counted.arrayBuffer())]).toEqual([0, 0x61, 0x73, 0x6d]);
    expect(seen.at(-1)).toEqual({ what: "core", received: 4, total: 4, done: true });
  });
});
