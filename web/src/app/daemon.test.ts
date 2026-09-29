import { describe, expect, test } from "bun:test";

import { daemonAttachPort, type HealthFetch, type PageLocation } from "./daemon";

const served: PageLocation = { protocol: "http:", hostname: "127.0.0.1", port: "8765" };

function answering(ok: boolean, body: unknown, seen: string[] = []): HealthFetch {
  return async (path, init) => {
    seen.push(`${path} ${init.credentials}`);
    return { ok, json: async () => body };
  };
}

describe("the daemon attach port", () => {
  test("is the page's own port when the daemon's health route answers with its session", async () => {
    const seen: string[] = [];
    expect(await daemonAttachPort(served, answering(true, { core: "pemu", protocol: 1 }, seen))).toBe(8765);
    expect(seen).toEqual(["/v1/health same-origin"]);
  });

  test("is null for a server that is not a daemon", async () => {
    expect(await daemonAttachPort(served, answering(false, null))).toBeNull();
    expect(await daemonAttachPort(served, answering(true, { core: "other" }))).toBeNull();
    expect(await daemonAttachPort(served, answering(true, "<html>"))).toBeNull();
    expect(
      await daemonAttachPort(served, async () => {
        throw new Error("offline");
      }),
    ).toBeNull();
  });

  test("is never asked of a page the attach URL could not reach with its cookie", async () => {
    const seen: string[] = [];
    const health = answering(true, { core: "pemu" }, seen);
    expect(await daemonAttachPort({ ...served, hostname: "localhost" }, health)).toBeNull();
    expect(await daemonAttachPort({ ...served, protocol: "https:" }, health)).toBeNull();
    expect(await daemonAttachPort({ ...served, protocol: "file:" }, health)).toBeNull();
    expect(seen).toEqual([]);
  });
});
