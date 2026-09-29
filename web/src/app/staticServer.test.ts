import { afterAll, describe, expect, test } from "bun:test";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { serveDir } from "../../tests/staticServer";

const dir = mkdtempSync(join(tmpdir(), "pemu-static-"));
writeFileSync(join(dir, "a.html"), "<p>a</p>");
afterAll(() => rmSync(dir, { recursive: true, force: true }));

describe("serveDir", () => {
  test("a malformed percent-encoding is a 400, and the server keeps serving", async () => {
    const served = await serveDir(dir, true);
    try {
      const bad = await fetch(`${served.url}%E0%A4%A`);
      expect(bad.status).toBe(400);
      const good = await fetch(`${served.url}a.html`);
      expect(good.status).toBe(200);
      expect(good.headers.get("cross-origin-embedder-policy")).toBe("require-corp");
      expect((await fetch(`${served.url}missing.html`)).status).toBe(404);
    } finally {
      await served.close();
    }
  });
});
