// The loopback file server the browser specs open their probe pages on, with the cross-origin
// isolation headers or without.
//
// Not `page.route(...).fulfill({ headers })`: Playwright WebKit (26.4, playwright 1.60) ignores
// `Cross-Origin-Opener-Policy` and `Cross-Origin-Embedder-Policy` on a fulfilled response, so the
// page stays unisolated, or stays isolated from a previous real navigation. Chromium honours them.
// Headers from a real HTTP response isolate both engines.

import { existsSync, readFileSync, statSync } from "node:fs";
import { createServer, type Server } from "node:http";
import { extname, join, posix } from "node:path";

const TYPES: Record<string, string> = {
  ".html": "text/html; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
  ".css": "text/css; charset=utf-8",
  ".wasm": "application/wasm",
  ".json": "application/json",
};

/** The two response headers that make a document cross-origin isolated. */
export const ISOLATION_HEADERS: Readonly<Record<string, string>> = {
  "cross-origin-opener-policy": "same-origin",
  "cross-origin-embedder-policy": "require-corp",
};

export interface StaticServer {
  readonly server: Server;
  readonly url: string;
  close(): Promise<void>;
}

/**
 * Serves `dir` on a free loopback port, plus `extra` (path without the leading slash to its body),
 * which wins over a file of the same name. Only files below `dir` are served.
 */
export async function serveDir(
  dir: string,
  isolated: boolean,
  extra: Readonly<Record<string, string | Uint8Array>> = {},
): Promise<StaticServer> {
  const server = createServer((request, response) => {
    let path: string;
    try {
      path = decodeURIComponent(new URL(request.url ?? "/", "http://x").pathname);
    } catch {
      // `%E0%A4%A` and the like: a URIError here would escape the handler and kill the server.
      response.writeHead(400).end("malformed percent-encoding");
      return;
    }
    // `/`-separated on every host, like the keys of `extra`.
    const relative = posix.normalize(path).replace(/^([/\\])+/, "");
    const headers = (name: string) => ({
      "content-type": TYPES[extname(name)] ?? "application/octet-stream",
      ...(isolated ? ISOLATION_HEADERS : {}),
    });
    const body = extra[relative];
    if (body !== undefined) {
      response.writeHead(200, headers(relative)).end(body);
      return;
    }
    const file = join(dir, relative);
    if (!file.startsWith(dir) || !existsSync(file) || !statSync(file).isFile()) {
      response.writeHead(404).end("not found");
      return;
    }
    response.writeHead(200, headers(file)).end(readFileSync(file));
  });
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  const address = server.address();
  const port = typeof address === "object" && address ? address.port : 0;
  return {
    server,
    url: `http://127.0.0.1:${port}/`,
    close: () =>
      new Promise<void>((resolve) => {
        server.closeAllConnections();
        server.close(() => resolve());
      }),
  };
}
