// The Cloudflare Worker script of the web bundle: a relay from the page's own origin to FoloToy's
// AI Passport site, for the play box (`app/play.ts`). That site answers only its own pages, so a
// browser on any other origin cannot read it; the page asks `/play-site/...` here instead.
//
// Static assets are served first and never reach this script. It runs for a path no asset
// matches, relays exactly two shapes of GET and answers 404 to everything else:
//
//   /play-site/api/plays/id/<number>   which firmware a play publishes
//   /play-site/api/download/<...>      that firmware
//
// It is an allowlist, not a proxy: the upstream host is fixed, no query and no header of the
// visitor's request is passed on (so no cookie), and the answer is rebuilt from the body and three
// headers. Nothing is stored or logged here. Plain JavaScript with no imports, shipped as written:
// `cargo xtask package` copies this file into the bundle (`xtask/src/package/cloudflare.rs`), and
// `tests/serve.ts --play-relay` runs the same function for `just run`.
//
// The default export is the only export: the Workers runtime takes every named export of its
// script for an entrypoint and refuses to start on one that is a string or a number. What the
// tests and the test server use is on that object, beside `fetch`.

/** The site relayed to. */
const UPSTREAM = "https://ai-passport.folotoy.cn";

/** The path prefix, on the page's origin, that this script answers. */
const RELAY_PREFIX = "/play-site";

/**
 * Set on every answer of the relay, so the page can tell "this server has no relay" (a static
 * server's own 404) from "the play site has no such play" (a relayed 404).
 */
const RELAY_HEADER = "x-play-relay";

/** The board's flash size: no firmware the page loads is larger. */
const MAX_FIRMWARE_BYTES = 8 * 1024 * 1024;

const ROUTES = [
  /^\/api\/plays\/id\/[1-9][0-9]{0,8}$/,
  // One to three plain segments, each starting with a letter or digit, so none is `.` or `..`.
  /^\/api\/download(?:\/[A-Za-z0-9][A-Za-z0-9._-]{0,127}){1,3}$/,
];

/** The upstream address a request path relays to, or `null` when the path is not relayed. */
function upstreamUrl(pathname) {
  if (!pathname.startsWith(`${RELAY_PREFIX}/`)) {
    return null;
  }
  const path = pathname.slice(RELAY_PREFIX.length);
  return ROUTES.some((route) => route.test(path)) ? `${UPSTREAM}${path}` : null;
}

function answer(status, body, headers = {}) {
  return new Response(body, {
    status,
    headers: {
      [RELAY_HEADER]: "1",
      "cache-control": "no-store",
      "x-content-type-options": "nosniff",
      "cross-origin-resource-policy": "same-origin",
      ...headers,
    },
  });
}

function refusal(status, detail) {
  return answer(status, JSON.stringify({ detail }), { "content-type": "application/json" });
}

/**
 * The relay's answer to `request`, or `null` when its path is outside {@link RELAY_PREFIX} and so
 * not the relay's to answer. `fetchUpstream` is `fetch`, injected so a test can be the play site.
 */
async function relay(request, fetchUpstream = fetch) {
  const url = new URL(request.url);
  if (url.pathname !== RELAY_PREFIX && !url.pathname.startsWith(`${RELAY_PREFIX}/`)) {
    return null;
  }
  const upstream = upstreamUrl(url.pathname);
  if (upstream === null) {
    return refusal(404, "not a path this relay serves");
  }
  if (request.method !== "GET") {
    return answer(405, JSON.stringify({ detail: "the relay answers GET only" }), { "content-type": "application/json", allow: "GET" });
  }
  // A browser says where a request comes from; another site's page is not relayed for. A client
  // that sends no such header (curl, a check script) is.
  const site = request.headers.get("sec-fetch-site");
  if (site !== null && site !== "same-origin") {
    return refusal(403, "the relay answers this site's own page only");
  }
  let response;
  try {
    // A fresh request: the fixed address and nothing of the visitor's. A redirect is not followed,
    // so the relay never asks a host other than the one named above.
    response = await fetchUpstream(upstream, { method: "GET", redirect: "manual", headers: { accept: "application/json, application/octet-stream" } });
  } catch (error) {
    return refusal(502, `${new URL(UPSTREAM).host} could not be reached: ${error instanceof Error ? error.message : String(error)}`);
  }
  if (response.status === 404) {
    await response.body?.cancel();
    return refusal(404, "the play site has no such play or firmware");
  }
  if (!response.ok) {
    await response.body?.cancel();
    return refusal(502, `${new URL(UPSTREAM).host} answered HTTP ${response.status}`);
  }
  const headers = { "content-type": response.headers.get("content-type") ?? "application/octet-stream" };
  // A length is passed on only when it counts the bytes of the body as sent here.
  const length = response.headers.get("content-length");
  const encoded = (response.headers.get("content-encoding") ?? "identity").trim().toLowerCase() !== "identity";
  if (length !== null && /^[0-9]+$/.test(length) && !encoded) {
    if (Number(length) > MAX_FIRMWARE_BYTES) {
      await response.body?.cancel();
      return refusal(502, `${new URL(UPSTREAM).host} answered with ${length} bytes, over the ${MAX_FIRMWARE_BYTES} the relay passes on`);
    }
    headers["content-length"] = length;
  }
  return answer(200, response.body, headers);
}

export default {
  /** The Worker's handler: the relay, and a plain 404 for any other path no asset matched. */
  async fetch(request) {
    return (await relay(request)) ?? new Response("not found", { status: 404, headers: { "x-content-type-options": "nosniff" } });
  },
  relay,
  upstreamUrl,
  UPSTREAM,
  RELAY_PREFIX,
  RELAY_HEADER,
  MAX_FIRMWARE_BYTES,
};
