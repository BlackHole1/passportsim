// `bun build` does not follow `new Worker(new URL(...))` into a chunk, so a bundle whose Worker URL
// names a file the build did not emit type-checks and passes every unit test, then does nothing
// in the browser. That shipped once, hence this test.

import { describe, expect, test } from "bun:test";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const WEB = resolve(dirname(fileURLToPath(import.meta.url)), "..", "..");

async function build(): Promise<{ dir: string; main: string }> {
  const dir = await mkdtemp(join(tmpdir(), "pemu-web-build-"));
  const steps = [
    ["bun", "build", "src/app/main.ts", "--outdir", dir, "--target", "browser", "--production"],
    ["bun", "build", "src/worker/worker.ts", "--outdir", dir, "--target", "browser"],
    ["bun", "build", "src/audio/worklet.ts", "--outdir", dir, "--target", "browser"],
    [join(WEB, "node_modules", ".bin", "tailwindcss"), "-i", "src/styles.css", "-o", join(dir, "styles.css")],
  ];
  for (const cmd of steps) {
    const built = Bun.spawnSync({ cmd, cwd: WEB });
    expect(built.exitCode).toBe(0);
  }
  return { dir, main: await Bun.file(join(dir, "main.js")).text() };
}

describe("the built bundle", () => {
  test("every Worker the page starts is a file the build emitted", async () => {
    const { dir, main } = await build();
    try {
      const urls = [...main.matchAll(/new Worker\(\s*new URL\(\s*"([^"]+)"/g)].map(
        (match) => match[1] ?? "",
      );
      // One dedicated Worker holds the core; a second one is a topology change, not a build detail.
      expect(urls).toHaveLength(1);
      for (const url of urls) {
        const target = join(dir, url.replace(/^\.\//, ""));
        expect(await Bun.file(target).exists()).toBe(true);
        expect(target.endsWith(".js")).toBe(true);
      }
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  }, 30_000);

  test("the Worker entry starts itself, so the page only has to construct it", async () => {
    const { dir } = await build();
    try {
      const worker = await Bun.file(join(dir, "worker.js")).text();
      expect(worker).toContain("onmessage");
      expect(worker.length).toBeGreaterThan(1_000);
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  }, 30_000);

  test("the audio worklet the page loads beside itself is a file the build emitted", async () => {
    const { dir, main } = await build();
    try {
      // `host.ts` resolves `worklet.js` against `main.js`, and `xtask/src/package/layout.rs` `WEB_FILES`
      // must ship it.
      expect(main).toContain("worklet.js");
      const worklet = await Bun.file(join(dir, "worklet.js")).text();
      expect(worklet).toContain("registerProcessor");
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  }, 30_000);

  test("the device photo is inside main.js, not a file beside it", async () => {
    const { dir, main } = await build();
    try {
      // `WEB_FILES` ships a fixed list, so an emitted image would be missing from every package.
      expect(main).toContain("data:image/webp;base64,");
      const emitted = [...new Bun.Glob("*.webp").scanSync(dir)];
      expect(emitted).toEqual([]);
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  }, 30_000);

  test("the stylesheet is compiled, with the theme tokens and the static badge's rule", async () => {
    const { dir } = await build();
    try {
      const css = await Bun.file(join(dir, "styles.css")).text();
      // Compiled, not copied: a source `@import "tailwindcss"` left in would fetch nothing offline.
      expect(css).not.toContain('@import "tailwindcss"');
      expect(css).toContain("--background");
      expect(css).toMatch(/\.emulator-badge\s*\{/);
      // No web font and no remote stylesheet: `passportsim serve` works offline.
      expect(css).not.toMatch(/@font-face|url\(\s*["']?https?:/);
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  }, 30_000);
});

describe("the files index.html names", () => {
  test("are copied by the build and shipped in every package", async () => {
    const html = await Bun.file(join(WEB, "public", "index.html")).text();
    const named = [...html.matchAll(/\b(?:href|src)="\.\/([^"]+)"/g)].map((match) => match[1] ?? "");
    expect(named).toContain("favicon.svg");
    const script = JSON.parse(await Bun.file(join(WEB, "package.json")).text()).scripts.build as string;
    const layout = await Bun.file(join(WEB, "..", "xtask", "src", "package", "layout.rs")).text();
    const list = /const WEB_FILES: \[&str; \d+\] = \[([^\]]*)\]/.exec(layout)?.[1] ?? "";
    const shipped = [...list.matchAll(/"([^"]+)"/g)].map((match) => match[1]);
    for (const file of named) {
      // Built by a step of its own, or copied from `public/`.
      const built = ["main.js", "styles.css"].includes(file);
      expect(built || script.includes(`cp public/${file} dist/`)).toBe(true);
      expect(shipped).toContain(file);
    }
  });
});
