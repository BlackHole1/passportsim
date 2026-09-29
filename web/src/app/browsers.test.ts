import { describe, expect, test } from "bun:test";
import { mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { basename, join } from "node:path";
import { BROWSER_ROWS, browserSkip, channelCandidates, missingBrowser, rowOf, runnableRows } from "../../tests/browsers";
import {
  corpusAbsence,
  coreCandidates,
  findCore,
  hasFlashArgs,
  imagePrecondition,
  imageSource,
} from "../../tests/preconditions";

function findCoreSkip(): string {
  const found = findCore({}, () => false);
  return "skip" in found ? found.skip : "";
}

describe("the browser matrix", () => {
  test("Chromium and Firefox run on every host, WebKit on macOS only, the installed Chrome and Edge on Windows only", () => {
    expect(runnableRows("darwin").map((row) => row.id)).toEqual(["chromium", "webkit", "firefox"]);
    expect(runnableRows("win32").map((row) => row.id)).toEqual([
      "chromium",
      "firefox",
      "windows-chrome",
      "windows-msedge",
    ]);
  });

  test("a missing Playwright Firefox is named with its install command", () => {
    const row = BROWSER_ROWS.find((r) => r.id === "firefox");
    if (row === undefined) throw new Error("no firefox row");
    expect(missingBrowser(row, {}, () => false)).toContain("bunx playwright install firefox");
    expect(missingBrowser(row, {}, () => true)).toBeNull();
  });

  test("the Windows rows are the Chromium engine through the chrome and msedge channels", () => {
    const windows = BROWSER_ROWS.filter((row) => row.host === "windows");
    expect(windows.map((row) => [row.engine, row.channel])).toEqual([
      ["chromium", "chrome"],
      ["chromium", "msedge"],
    ]);
  });

  test("a project's fixtures name exactly one row, so a skip never crosses to a sibling channel", () => {
    expect(rowOf("chromium", undefined)?.id).toBe("chromium");
    expect(rowOf("chromium", "")?.id).toBe("chromium");
    expect(rowOf("chromium", "chrome")?.id).toBe("windows-chrome");
    expect(rowOf("chromium", "msedge")?.id).toBe("windows-msedge");
    expect(rowOf("webkit", undefined)?.id).toBe("webkit");
    expect(rowOf("firefox", undefined)?.id).toBe("firefox");
  });

  test("a channel row looks for the installed browser where the installers put it", () => {
    const env = { LOCALAPPDATA: "C:\\U\\L", ProgramFiles: "C:\\PF", "ProgramFiles(x86)": "C:\\PF86" };
    const chrome = channelCandidates("chrome", env);
    expect(chrome).toHaveLength(3);
    expect(chrome[0]).toMatch(/^C:\\U\\L[\\/]Google[\\/]Chrome[\\/]Application[\\/]chrome\.exe$/);
    expect(chrome[1]).toMatch(/^C:\\PF[\\/]Google/);
    const edge = channelCandidates("msedge", env);
    expect(edge[0]).toMatch(/^C:\\PF86[\\/]Microsoft[\\/]Edge[\\/]Application[\\/]msedge\.exe$/);
    // A variable the host does not set drops its candidate instead of making a relative path.
    expect(channelCandidates("chrome", { ProgramFiles: "C:\\PF" })).toHaveLength(1);
    expect(channelCandidates("msedge", {})).toEqual([]);
  });

  test("a missing channel browser names the row and every place it looked; a present one is null", () => {
    const row = BROWSER_ROWS.find((r) => r.id === "windows-msedge");
    if (row === undefined) throw new Error("no windows-msedge row");
    const env = { "ProgramFiles(x86)": "C:\\PF86" };
    const gap = missingBrowser(row, env, () => false);
    expect(gap).toContain("Microsoft Edge");
    expect(gap).toContain("windows-msedge");
    expect(gap).toContain("msedge.exe");
    expect(missingBrowser(row, env, (path) => path.endsWith("msedge.exe"))).toBeNull();
    expect(missingBrowser(row, {}, () => true)).toContain("any location this host names");
  });
});

describe("image preconditions", () => {
  const PK_BOOT_ROW = { name: "the pk boot", waitsOn: "the pk boot" };

  test("a row skips only for a named, checkable absence", () => {
    const noCore = imagePrecondition("pk", PK_BOOT_ROW, { core: null, coreSkip: findCoreSkip(), env: {} });
    expect(noCore).toMatchObject({ kind: "skip", reason: expect.stringContaining("pemu_wasm.wasm") });
    const noImage = imagePrecondition("pk", PK_BOOT_ROW, { core: "/core.wasm", env: {} });
    expect(noImage).toMatchObject({ kind: "skip", reason: expect.stringContaining("PEMU_E2E_IMAGE_PK") });
  });

  test("with the core and a corpus here, a missing image is recorded BLOCKED on the row: its reason starts with `blocked: `", () => {
    const env = { PASSPORTSIM_DATA_ROOT: "/data" };
    const seen: string[] = [];
    const pre = imagePrecondition("pk", PK_BOOT_ROW, { core: "/core.wasm", env }, (path) => {
      seen.push(path);
      return true;
    });
    expect(pre.kind === "skip" ? pre.reason : "").toMatch(/^blocked: the pk boot: .*PEMU_E2E_IMAGE_PK.*the pk boot$/);
    expect(seen.map((path) => path.replaceAll("\\", "/"))).toEqual(["/data/corpus/pk"]);
  });

  test("with the core and no corpus on this host, a missing image is SKIPPED-CORPUS in the Rust corpus skips' words", () => {
    // No data root, blank or unset alike, as `pemu_testkit::corpus::data_root_from_env` reads it.
    for (const env of [{}, { PASSPORTSIM_DATA_ROOT: "" }, { PASSPORTSIM_DATA_ROOT: "  " }]) {
      const pre = imagePrecondition("pk", PK_BOOT_ROW, { core: "/core.wasm", env }, () => true);
      const reason = pre.kind === "skip" ? pre.reason : "";
      expect(reason).toStartWith("corpus id `pk` unavailable: no data root: set PASSPORTSIM_DATA_ROOT");
      expect(reason).toContain("PEMU_E2E_IMAGE_PK");
      expect(reason).not.toMatch(/^blocked: /);
    }
    // A data root with no `corpus/pk/` directory: nothing could have given the image either.
    const noDir = imagePrecondition("pk", PK_BOOT_ROW, { core: "/core.wasm", env: { PASSPORTSIM_DATA_ROOT: "/data" } }, () => false);
    expect(noDir.kind === "skip" ? noDir.reason : "").toStartWith(
      "corpus id `pk` unavailable: no `corpus/pk` directory below the data root",
    );
    expect(corpusAbsence("pk", { PASSPORTSIM_DATA_ROOT: "/data" }, () => true)).toBeNull();
  });

  test("with the core and an image given, the row runs on the files the page's loader will see", () => {
    const dir = mkdtempSync(join(tmpdir(), "pemu-image-"));
    writeFileSync(join(dir, "FoloToy-AI-Passport-8MB.bin"), new Uint8Array([0xe9]));
    writeFileSync(join(dir, "FoloToy-AI-Passport.elf"), new Uint8Array([0x7f, 0x45, 0x4c, 0x46]));
    const pre = imagePrecondition("demo", PK_BOOT_ROW, { core: "/core.wasm", env: { PEMU_E2E_IMAGE_DEMO: dir } });
    expect(pre.kind).toBe("run");
    if (pre.kind !== "run") {
      return;
    }
    expect(pre.source.directory).toBe(true);
    expect(pre.source.name).toBe(basename(dir));
    expect(pre.source.files.map((file) => file.relative).sort()).toEqual([
      "FoloToy-AI-Passport-8MB.bin",
      "FoloToy-AI-Passport.elf",
    ]);
    expect(hasFlashArgs(pre.source)).toBe(false);
  });

  test("a single file is a rootless drop named after the file, and a build directory is recognized", () => {
    const dir = mkdtempSync(join(tmpdir(), "pemu-image-"));
    const bin = join(dir, "merged.bin");
    writeFileSync(bin, new Uint8Array([0xe9]));
    const file = imageSource("pk", { PEMU_E2E_IMAGE_PK: bin });
    expect(file).toMatchObject({ directory: false, name: "merged" });
    expect(file?.files.map((entry) => entry.relative)).toEqual(["merged.bin"]);

    writeFileSync(join(dir, "flash_args"), "0x0 merged.bin\n");
    const build = imageSource("pk", { PEMU_E2E_IMAGE_PK: dir });
    expect(build !== null && hasFlashArgs(build)).toBe(true);
  });

  test("an image the variable names and this host does not have throws, like a missing core", () => {
    expect(() => imageSource("pk", { PEMU_E2E_IMAGE_PK: "/images/nothing-here" })).toThrow(
      "PEMU_E2E_IMAGE_PK names /images/nothing-here",
    );
  });
});

describe("where the specs find the wasm core", () => {
  test("PEMU_E2E_CORE wins and is the only candidate, and a missing one throws instead of skipping", () => {
    expect(coreCandidates({ PEMU_E2E_CORE: "/ci/pemu_wasm.wasm" })).toEqual(["/ci/pemu_wasm.wasm"]);
    expect(findCore({ PEMU_E2E_CORE: "/ci/pemu_wasm.wasm" }, (path) => path === "/ci/pemu_wasm.wasm")).toEqual({
      path: "/ci/pemu_wasm.wasm",
    });
    expect(() => findCore({ PEMU_E2E_CORE: "/ci/pemu_wasm.wasm" }, () => false)).toThrow(
      "PEMU_E2E_CORE names /ci/pemu_wasm.wasm, which does not exist",
    );
  });

  test("without it, cargo's wasm-release output, then a packaged dist/", () => {
    const candidates = coreCandidates({});
    expect(candidates).toHaveLength(2);
    expect(candidates[0]).toMatch(/target[\\/]wasm32-unknown-unknown[\\/]wasm-release[\\/]pemu_wasm\.wasm$/);
    expect(candidates[1]).toMatch(/web[\\/]dist[\\/]pemu_wasm\.wasm$/);
    expect(coreCandidates({ CARGO_TARGET_DIR: "/tmp/t" })[0]).toMatch(/^[\\/]tmp[\\/]t[\\/]wasm32-unknown-unknown/);
    const dist = candidates[1] ?? "";
    expect(findCore({}, (path) => path === dist)).toEqual({ path: dist });
  });
});

describe("the browser rows", () => {
  test("PEMU_E2E_REQUIRE_BROWSERS turns a missing browser from a skip into a failure", () => {
    expect(browserSkip("not installed", {})).toBe("not installed");
    expect(browserSkip("not installed", { PEMU_E2E_REQUIRE_BROWSERS: "1" })).toBeNull();
    expect(browserSkip(null, {})).toBeNull();
  });
});
