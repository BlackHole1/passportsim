// A bundle served from a subdirectory boots. The core was once resolved as `../../pemu_wasm.wasm`
// from the Worker's URL, which in the built bundle clamps to the origin root. These tests serve the
// bundle from prefixed directories and drive the Worker's own loader against them; the last builds
// the Worker as `package.json` does and starts it, since Bun cannot start a Worker from `http:`.

import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { WasmCore } from "./core";
import { abiCore, NOW_PS } from "./abiCoreFixture";
import { ABI_VERSION } from "./layout";
import { CORE_FILE, bundledCoreUrl, loadBundledCore, type FromWorker } from "./worker";

const WEB = resolve(dirname(fileURLToPath(import.meta.url)), "..", "..");

let server: ReturnType<typeof Bun.serve>;
const CORE = abiCore();
const requested: string[] = [];

/**
 * The directories a copy of the bundle sits in, as a static host with the packaged directory
 * (`worker.js` beside `pemu_wasm.wasm`) copied under each would serve it.
 */
const BUNDLE_DIRS = ["/", "/emu/", "/a/deeper/emu/"];

beforeAll(() => {
  server = Bun.serve({
    port: 0,
    fetch(request) {
      const path = new URL(request.url).pathname;
      requested.push(path);
      for (const dir of BUNDLE_DIRS) {
        if (path === `${dir}worker.js`) {
          return new Response("// worker", { headers: { "content-type": "text/javascript" } });
        }
        if (path === `${dir}${CORE_FILE}`) {
          return new Response(CORE, { headers: { "content-type": "application/wasm" } });
        }
      }
      return new Response("not found", { status: 404 });
    },
  });
});

afterAll(() => {
  server.stop(true);
});

function workerScript(dir: string): string {
  return new URL(`${dir}worker.js`, server.url).href;
}

describe("a bundle served under a path prefix", () => {
  test("the hand-assembled core is a valid module", () => {
    expect(WebAssembly.validate(CORE)).toBe(true);
  });

  for (const dir of BUNDLE_DIRS) {
    test(`boots from ${dir}`, async () => {
      expect((await fetch(workerScript(dir))).status).toBe(200);
      requested.length = 0;
      const core = await loadBundledCore(bundledCoreUrl(workerScript(dir)), "{}");
      expect(core).toBeInstanceOf(WasmCore);
      expect(core.nowPs()).toBe(NOW_PS);
      expect(requested).toEqual([`${dir}${CORE_FILE}`]);
      core.drop();
    });
  }

  test("the resolution keeps the page's query out of the core's URL", async () => {
    requested.length = 0;
    const core = await loadBundledCore(bundledCoreUrl(`${workerScript("/emu/")}?v=3#x`), "{}");
    expect(core.nowPs()).toBe(NOW_PS);
    expect(requested).toEqual([`/emu/${CORE_FILE}`]);
  });
});

describe("the resolution this package replaced", () => {
  test("clamps to the origin root, where a prefixed bundle has no core", async () => {
    const onlyEmu = Bun.serve({
      port: 0,
      fetch(request) {
        const path = new URL(request.url).pathname;
        requested.push(path);
        return path === `/emu/${CORE_FILE}`
          ? new Response(CORE, { headers: { "content-type": "application/wasm" } })
          : new Response("not found", { status: 404 });
      },
    });
    try {
      const script = new URL("/emu/worker.js", onlyEmu.url).href;
      const old = new URL(`../../${CORE_FILE}`, script).href;
      requested.length = 0;
      await expect(loadBundledCore(old, "{}")).rejects.toThrow();
      expect(requested).toEqual([`/${CORE_FILE}`]);
      requested.length = 0;
      const core = await loadBundledCore(bundledCoreUrl(script), "{}");
      expect(core.nowPs()).toBe(NOW_PS);
      expect(requested).toEqual([`/emu/${CORE_FILE}`]);
    } finally {
      onlyEmu.stop(true);
    }
  });
});

describe("the built Worker bundle", () => {
  test("boots with its core beside it in a nested directory", async () => {
    const root = await mkdtemp(join(tmpdir(), "pemu-subpath-"));
    try {
      const dir = join(root, "site", "emu");
      await mkdir(dir, { recursive: true });
      const built = Bun.spawnSync({
        cmd: [
          "bun",
          "build",
          "src/worker/worker.ts",
          "--outdir",
          dir,
          "--target",
          "browser",
          "--minify",
        ],
        cwd: WEB,
      });
      expect(built.exitCode).toBe(0);
      await writeFile(join(dir, CORE_FILE), abiCore());

      const worker = new Worker(join(dir, "worker.js"), { type: "module" });
      try {
        // The core's download reports come first, then the boot's answer.
        const downloads: FromWorker[] = [];
        const first = await Promise.race([
          new Promise<FromWorker>((settle) => {
            worker.onmessage = (event: MessageEvent<FromWorker>) => {
              if (event.data.type === "download") {
                downloads.push(event.data);
              } else {
                settle(event.data);
              }
            };
            worker.postMessage({ type: "boot", config: "{}" });
          }),
          Bun.sleep(10_000).then(() => ({ type: "error", message: "no answer" }) as FromWorker),
        ]);
        expect(first).toEqual({ type: "ready", abiVersion: ABI_VERSION });
        expect(downloads[0]).toEqual({ type: "download", what: "core", received: 0, total: null, done: false });
        expect(downloads.at(-1)).toMatchObject({ type: "download", what: "core", done: true });
        expect(downloads.at(-1)).not.toHaveProperty("error");
      } finally {
        worker.terminate();
      }
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  }, 30_000);
});
