// The relay with a fake play site behind it: what it passes on, and everything it does not.

import { describe, expect, test } from "bun:test";
import worker, { type UpstreamFetch } from "./playRelay.js";

const { MAX_FIRMWARE_BYTES, relay, RELAY_HEADER, RELAY_PREFIX, UPSTREAM, upstreamUrl } = worker;

const PAGE = "https://passportsim.test";

function upstream(answer: () => Response | Promise<Response>) {
  const asked: { url: string; init: unknown }[] = [];
  const fetchUpstream: UpstreamFetch = async (url, init) => {
    asked.push({ url, init });
    return answer();
  };
  return { asked, fetchUpstream };
}

function get(path: string, headers: Record<string, string> = {}): Request {
  return new Request(`${PAGE}${path}`, { headers });
}

describe("upstreamUrl", () => {
  test("relays a play's API path and a firmware's download path to the play site", () => {
    expect(upstreamUrl("/play-site/api/plays/id/1039")).toBe("https://ai-passport.folotoy.cn/api/plays/id/1039");
    expect(upstreamUrl("/play-site/api/download/community/community-b6eb4756")).toBe(
      "https://ai-passport.folotoy.cn/api/download/community/community-b6eb4756",
    );
    expect(upstreamUrl("/play-site/api/download/official/ai-passport/1.2.0")).toBe("https://ai-passport.folotoy.cn/api/download/official/ai-passport/1.2.0");
  });

  test("relays nothing else", () => {
    for (const path of [
      "/",
      "/play-site",
      "/play-site/",
      "/play-site/api/me",
      "/play-site/api/session",
      "/play-site/api/plays/id/",
      "/play-site/api/plays/id/0",
      "/play-site/api/plays/id/abc",
      "/play-site/api/plays/id/1039/",
      "/play-site/api/plays/id/1039/extra",
      "/play-site/api/plays/community-b6eb4756",
      "/play-site/api/download",
      "/play-site/api/download/",
      "/play-site/api/download/..",
      "/play-site/api/download/../me",
      "/play-site/api/download/a/../../me",
      "/play-site/api/download/a/b/c/d",
      "/play-site/api/download/a//b",
      "/play-site/api/download/a/%2e%2e",
      "/play-site/plays/1039/",
      "/api/plays/id/1039",
      "/play-sitex/api/plays/id/1039",
    ]) {
      expect(upstreamUrl(path), path).toBeNull();
    }
  });
});

describe("relay", () => {
  test("passes a play's answer on with the relay's own headers and none of the play site's", async () => {
    const body = JSON.stringify({ ok: true, play: { id: 1039 } });
    const site = upstream(
      () =>
        new Response(body, {
          headers: {
            "content-type": "application/json",
            "content-length": String(body.length),
            "set-cookie": "session=1",
            "content-security-policy": "frame-ancestors 'self'",
            "access-control-allow-credentials": "true",
          },
        }),
    );
    const response = (await relay(get("/play-site/api/plays/id/1039?utm=1", { cookie: "mine=1", authorization: "Bearer x", "sec-fetch-site": "same-origin" }), site.fetchUpstream)) as Response;
    expect(response.status).toBe(200);
    expect(await response.text()).toBe(body);
    expect(Object.fromEntries(response.headers)).toEqual({
      "x-play-relay": "1",
      "cache-control": "no-store",
      "x-content-type-options": "nosniff",
      "cross-origin-resource-policy": "same-origin",
      "content-type": "application/json",
      "content-length": String(body.length),
    });
    // The fixed address, with no query and nothing of the visitor's request.
    expect(site.asked).toEqual([
      {
        url: "https://ai-passport.folotoy.cn/api/plays/id/1039",
        init: { method: "GET", redirect: "manual", headers: { accept: "application/json, application/octet-stream" } },
      },
    ]);
  });

  test("passes a firmware on as bytes, with its length when the play site states one", async () => {
    const bytes = new Uint8Array([0xe9, 1, 2, 3]);
    const site = upstream(
      () => new Response(bytes, { headers: { "content-type": "application/octet-stream", "content-length": "4", "content-disposition": 'attachment; filename="x.bin"' } }),
    );
    const response = (await relay(get("/play-site/api/download/community/community-b6eb4756"), site.fetchUpstream)) as Response;
    expect(response.status).toBe(200);
    expect(response.headers.get("content-type")).toBe("application/octet-stream");
    expect(response.headers.get("content-length")).toBe("4");
    expect(response.headers.get("content-disposition")).toBeNull();
    expect(new Uint8Array(await response.arrayBuffer())).toEqual(bytes);
  });

  test("states no length for a body the play site sent encoded, and refuses one over the flash size", async () => {
    const encoded = upstream(() => new Response("{}", { headers: { "content-length": "2", "content-encoding": "gzip" } }));
    const passed = (await relay(get("/play-site/api/plays/id/7"), encoded.fetchUpstream)) as Response;
    expect(passed.status).toBe(200);
    expect(passed.headers.get("content-length")).toBeNull();
    expect(passed.headers.get("content-encoding")).toBeNull();

    const huge = upstream(() => new Response("x", { headers: { "content-length": String(MAX_FIRMWARE_BYTES + 1) } }));
    const refused = (await relay(get("/play-site/api/download/community/big"), huge.fetchUpstream)) as Response;
    expect(refused.status).toBe(502);
    expect(await refused.json()).toEqual({ detail: `ai-passport.folotoy.cn answered with ${MAX_FIRMWARE_BYTES + 1} bytes, over the ${MAX_FIRMWARE_BYTES} the relay passes on` });
  });

  test("a body with no stated length is cut off past the flash size, and passed whole up to it", async () => {
    const chunk = new Uint8Array(1024 * 1024);
    const streamed = (chunks: number, extra: number) =>
      upstream(() => {
        let sent = 0;
        return new Response(
          new ReadableStream<Uint8Array>({
            pull(controller) {
              if (sent < chunks) {
                controller.enqueue(chunk);
              } else if (sent === chunks && extra > 0) {
                controller.enqueue(new Uint8Array(extra));
              } else {
                controller.close();
              }
              sent += 1;
            },
          }),
        );
      });
    const whole = MAX_FIRMWARE_BYTES / chunk.length;

    const atLimit = (await relay(get("/play-site/api/download/community/full"), streamed(whole, 0).fetchUpstream)) as Response;
    expect([atLimit.status, atLimit.headers.get("content-length")]).toEqual([200, null]);
    expect((await atLimit.arrayBuffer()).byteLength).toBe(MAX_FIRMWARE_BYTES);

    const over = (await relay(get("/play-site/api/download/community/endless"), streamed(whole, 1).fetchUpstream)) as Response;
    expect(over.status).toBe(200);
    let failure = "";
    try {
      await over.arrayBuffer();
    } catch (error) {
      failure = error instanceof Error ? error.message : String(error);
    }
    expect(failure).toBe(`the play site sent more than the ${MAX_FIRMWARE_BYTES} bytes the relay passes on`);
  });

  test("a play the site does not have is a 404 that carries the relay's mark", async () => {
    const site = upstream(() => new Response(JSON.stringify({ detail: "玩法不存在" }), { status: 404 }));
    const response = (await relay(get("/play-site/api/plays/id/999999"), site.fetchUpstream)) as Response;
    expect(response.status).toBe(404);
    expect(response.headers.get(RELAY_HEADER)).toBe("1");
    expect(await response.json()).toEqual({ detail: "the play site has no such play or firmware" });
  });

  test("an error, a redirect and an unreachable play site are each a 502 that says which", async () => {
    const failing = (await relay(get("/play-site/api/plays/id/1"), upstream(() => new Response("busy", { status: 503 })).fetchUpstream)) as Response;
    expect([failing.status, await failing.json()]).toEqual([502, { detail: "ai-passport.folotoy.cn answered HTTP 503" }]);
    const moved = (await relay(
      get("/play-site/api/download/community/moved"),
      upstream(() => new Response(null, { status: 302, headers: { location: "https://example.com/fw.bin" } })).fetchUpstream,
    )) as Response;
    expect([moved.status, await moved.json()]).toEqual([502, { detail: "ai-passport.folotoy.cn answered HTTP 302" }]);
    const down = (await relay(get("/play-site/api/plays/id/1"), () => Promise.reject(new Error("connection refused")))) as Response;
    expect([down.status, await down.json()]).toEqual([502, { detail: "ai-passport.folotoy.cn could not be reached: connection refused" }]);
    for (const response of [failing, moved, down]) {
      expect(response.headers.get(RELAY_HEADER)).toBe("1");
    }
  });

  test("asks the play site nothing for a path it does not serve, a method that is not GET, or another site's page", async () => {
    const site = upstream(() => new Response("{}"));
    const stray = (await relay(get("/play-site/api/me"), site.fetchUpstream)) as Response;
    expect([stray.status, stray.headers.get(RELAY_HEADER)]).toEqual([404, "1"]);
    const root = (await relay(get("/play-site/"), site.fetchUpstream)) as Response;
    expect([root.status, root.headers.get(RELAY_HEADER)]).toEqual([404, "1"]);
    for (const method of ["POST", "PUT", "DELETE", "HEAD", "OPTIONS"]) {
      const response = (await relay(new Request(`${PAGE}/play-site/api/plays/id/1039`, { method }), site.fetchUpstream)) as Response;
      expect([method, response.status, response.headers.get("allow")]).toEqual([method, 405, "GET"]);
    }
    for (const from of ["cross-site", "same-site", "none"]) {
      const response = (await relay(get("/play-site/api/plays/id/1039", { "sec-fetch-site": from }), site.fetchUpstream)) as Response;
      expect([from, response.status]).toEqual([from, 403]);
    }
    expect(site.asked).toEqual([]);
  });

  test("a path outside the prefix is not the relay's, and the Worker answers it 404 without the mark", async () => {
    const site = upstream(() => new Response("{}"));
    for (const path of ["/", "/missing.js", "/api/plays/id/1039", "/play-sitex/api/plays/id/1039"]) {
      expect(await relay(get(path), site.fetchUpstream), path).toBeNull();
      const response = await worker.fetch(get(path));
      expect([path, response.status, response.headers.get(RELAY_HEADER)]).toEqual([path, 404, null]);
    }
    expect(site.asked).toEqual([]);
  });

  test("the prefix and the play site are the ones the page uses", async () => {
    const { PLAY_RELAY_PATH, PLAY_SITE } = await import("../app/play");
    expect(RELAY_PREFIX).toBe(`/${PLAY_RELAY_PATH}`);
    expect(UPSTREAM).toBe(PLAY_SITE);
  });

  test("the script has one export, the default: the Workers runtime refuses any other that is not a handler", async () => {
    expect(Object.keys(await import("./playRelay.js"))).toEqual(["default"]);
    expect(typeof worker.fetch).toBe("function");
  });
});
