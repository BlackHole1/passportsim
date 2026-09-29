// The static server the Playwright suites load the built page from: `web/dist` on 127.0.0.1 only,
// with the two headers that make the page cross-origin isolated, since the pacing and audio paths
// use SharedArrayBuffers only then. Usage: `bun tests/serve.ts [port]`.
//
// Beside `dist/` it serves what a release bundle carries, when this host has it:
// - `/pemu_wasm.wasm`: the core `preconditions.ts` `findCore` finds, even when `dist/` holds one;
// - `/official.pebundle`: built from the corpus by `demoBundle.ts`.
// Each absence is printed, and the page then reports the missing file itself.
//
// It also publishes each `PEMU_E2E_IMAGE_<NAME>` firmware under `/e2e-image/<id>/<file>` for the
// drag-and-drop test of `loader.spec.ts`. Only the files the variable's path holds are routed, by
// the exact relative name the loader sees; the host path never appears in a URL, listing or error
// body, and the images are never copied.

import { existsSync, readFileSync, statSync } from "node:fs";
import { extname, join, posix } from "node:path";
import { pebundle, readOfficialFiles } from "./demoBundle";
import { CORE_FILE, findCore, IMAGE_ROUTE, imageSource, imageUrl, imageVariable } from "./preconditions";

const DIST = join(import.meta.dir, "..", "dist");
const PORT = Number(process.argv[2] ?? process.env.PEMU_WEB_PORT ?? 4173);

const TYPES: Record<string, string> = {
  ".html": "text/html; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
  ".css": "text/css; charset=utf-8",
  ".wasm": "application/wasm",
  ".json": "application/json",
  ".svg": "image/svg+xml",
  ".png": "image/png",
  ".pebundle": "application/octet-stream",
};

/** Files served from memory or from outside `dist/`, by path without the leading slash. */
const extra = new Map<string, Buffer>();

// `extra` is served before `dist/`, so the core found wins over one left by an earlier packaging run.
const core = findCore(process.env);
if ("path" in core) {
  extra.set(CORE_FILE, readFileSync(core.path));
  console.log(`serving the wasm core ${core.path}`);
} else if ("skip" in core) {
  console.log(core.skip);
}
const official = readOfficialFiles();
if ("files" in official) {
  extra.set("official.pebundle", pebundle(official.files));
  console.log("serving official.pebundle built from the corpus");
} else {
  console.log("mismatch" in official ? official.mismatch : official.skip);
}

// `/e2e-image/<id>/<relative>` to its host path, built once so a request is a lookup, never a join.
const images = new Map<string, string>();
for (const [key, value] of Object.entries(process.env)) {
  if (!key.startsWith("PEMU_E2E_IMAGE_") || value === undefined || value === "") {
    continue;
  }
  const id = key.slice("PEMU_E2E_IMAGE_".length).toLowerCase();
  if (imageVariable(id) !== key) {
    console.log(`${key} names no image id this server can publish; skipped`);
    continue;
  }
  try {
    const source = imageSource(id, process.env);
    if (source === null) {
      continue;
    }
    for (const file of source.files) {
      images.set(imageUrl(id, file.relative).slice(1), file.path);
    }
    console.log(`serving the \`${id}\` image as ${source.files.length} file(s) under ${IMAGE_ROUTE}/${id}/`);
  } catch (error) {
    console.log(`${key}: ${error instanceof Error ? error.message : String(error)}`);
  }
}

Bun.serve({
  hostname: "127.0.0.1",
  port: PORT,
  fetch(request) {
    let path: string;
    try {
      path = decodeURIComponent(new URL(request.url).pathname);
    } catch {
      return new Response("malformed percent-encoding", { status: 400 });
    }
    // A URL path is `/`-separated on every host; `path.normalize` would give `\` on Windows.
    const relative = posix.normalize(path === "/" ? "/index.html" : path).replace(/^([/\\])+/, "");
    const headers = {
      "content-type": TYPES[extname(relative)] ?? "application/octet-stream",
      "cross-origin-opener-policy": "same-origin",
      "cross-origin-embedder-policy": "require-corp",
    };
    const body = extra.get(relative);
    if (body !== undefined) {
      return new Response(new Uint8Array(body), { headers });
    }
    const image = images.get(relative);
    if (image !== undefined) {
      return new Response(Bun.file(image), { headers });
    }
    const file = join(DIST, relative);
    if (!file.startsWith(DIST) || !existsSync(file) || !statSync(file).isFile()) {
      return new Response("not found", { status: 404 });
    }
    return new Response(Bun.file(file), { headers });
  },
});
console.log(`serving ${DIST} on http://127.0.0.1:${PORT}/`);
