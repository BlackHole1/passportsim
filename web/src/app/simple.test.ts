import { describe, expect, test } from "bun:test";
import { RingId } from "../worker/layout";
import { createPage, type Page } from "./page";
import { PREF_KEYS, type StorageLike } from "./prefs";
import { installDom, settle } from "./view/testDom";

const window = installDom();
const document = window.document as unknown as Document;

interface Mounted {
  readonly page: Page;
  readonly mount: HTMLElement;
  readonly toWorker: unknown[];
}

function mountPage(
  options: {
    storage?: () => StorageLike | null;
    search?: string;
    systemLocale?: string;
    transport?: (request: string) => Promise<{ ok: string }>;
  } = {},
): Mounted {
  const mount = document.createElement("div");
  document.body.appendChild(mount);
  const toWorker: unknown[] = [];
  let clock = 0;
  const page = createPage({
    mount,
    transport: options.transport ?? (() => Promise.resolve({ ok: JSON.stringify({ json: {}, text: "" }) })),
    toWorker: (message) => toWorker.push(message),
    rewindSource: { snapshot: () => new Uint8Array(0), frame: () => null },
    now: () => (clock += 3),
    confirm: () => true,
    viewport: () => ({ width: 1440, height: 900 }),
    devicePixelRatio: () => 1,
    storage: options.storage,
    search: options.search,
    systemLocale: () => options.systemLocale ?? "en-US",
  });
  return { page, mount, toWorker };
}

function memory(): StorageLike & { readonly map: Map<string, string> } {
  const map = new Map<string, string>();
  return {
    map,
    getItem: (key) => map.get(key) ?? null,
    setItem: (key, value) => {
      map.set(key, value);
    },
  };
}

const modeOf = (mount: HTMLElement) => mount.querySelector("[data-mode]")?.getAttribute("data-mode");

describe("the mode", () => {
  test("a first visit opens in simple mode: the device, the firmware card and the log, no workbench", () => {
    const { mount } = mountPage({ storage: () => memory() });
    expect(modeOf(mount)).toBe("simple");
    expect(mount.querySelector("[data-control=ok]")).not.toBeNull();
    expect(mount.querySelector(".firmware-card [data-loader-input=files]")).not.toBeNull();
    expect(mount.querySelector(".log-panel")).not.toBeNull();
    expect((mount.querySelector(".workbench") as HTMLElement).hidden).toBe(true);
  });

  test("the switch shows the workbench and is remembered for the next load", async () => {
    const storage = memory();
    const first = mountPage({ storage: () => storage });
    first.page.model.actions.setMode("advanced");
    await settle();
    expect(modeOf(first.mount)).toBe("advanced");
    expect((first.mount.querySelector(".workbench") as HTMLElement).hidden).toBe(false);
    expect(first.mount.querySelector(".log-panel")).toBeNull();
    expect(storage.map.get(PREF_KEYS.mode)).toBe("advanced");

    const second = mountPage({ storage: () => storage });
    expect(modeOf(second.mount)).toBe("advanced");
  });

  test("with storage that throws, the page mounts and the switch still works for this load", async () => {
    const { page, mount } = mountPage({
      storage: () => {
        throw new Error("SecurityError: the operation is insecure");
      },
    });
    expect(modeOf(mount)).toBe("simple");
    page.model.actions.setMode("advanced");
    await settle();
    expect(modeOf(mount)).toBe("advanced");
  });

  test("a `?mode=` parameter opens that mode and is not written back", () => {
    const storage = memory();
    const { mount } = mountPage({ storage: () => storage, search: "?mode=advanced" });
    expect(modeOf(mount)).toBe("advanced");
    expect(storage.map.has(PREF_KEYS.mode)).toBe(false);
  });
});

describe("the language", () => {
  test("follows the system, sets <html lang>, and a choice is remembered", async () => {
    const storage = memory();
    const { page, mount } = mountPage({ storage: () => storage, systemLocale: "fr-FR" });
    await settle();
    expect(document.documentElement.lang).toBe("fr");
    expect(mount.querySelector("[data-control=ok]")?.getAttribute("aria-label")).toBe("Bouton OK");
    // Only a switch stores a language: a first visit that stored what it detected would stop
    // following the system for good.
    expect(storage.map.has(PREF_KEYS.locale)).toBe(false);

    page.model.actions.setLocale("ja");
    await settle();
    expect(document.documentElement.lang).toBe("ja");
    expect(mount.querySelector("[data-control=ok]")?.getAttribute("aria-label")).toBe("OK ボタン");
    expect(storage.map.get(PREF_KEYS.locale)).toBe("ja");
  });
});

describe("a load in simple mode", () => {
  function mergedBin(): Uint8Array {
    const bytes = new Uint8Array(0x20_000);
    bytes[0] = 0xe9;
    return bytes;
  }

  const pageLines = (mount: HTMLElement) =>
    [...mount.querySelectorAll(".log-panel .log-page")].map((line) => line.textContent?.replace(/^\s*\+\d+ ms/, "").trim());

  test("the log says each step as it happens, then shows the new firmware's console", async () => {
    const { page, mount, toWorker } = mountPage({ storage: () => memory() });
    await settle();
    expect(pageLines(mount)).toEqual(["› Starting the bundled demo firmware"]);

    const offered = page.loader.offer({
      root: null,
      files: [{ path: "hello.bin", size: 0x20_000, read: () => Promise.resolve(mergedBin()) }],
    });
    await settle();
    await settle();
    const boot = toWorker.find((message) => (message as { type?: string }).type === "boot") as { token: string };
    expect(pageLines(mount)).toEqual([
      "› Received 1 file(s), 128.0 KiB in total",
      // A lone file is recognised by its first bytes, so it is read before it is named.
      "› Read hello.bin (128.0 KiB)",
      "› Detected a merged flash image",
      "› Handed hello to the emulator core",
    ]);
    expect(mount.querySelector("[data-loader]")?.getAttribute("data-loader-state")).toBe("loading");

    page.ready(boot.token);
    await offered;
    await settle();
    expect(pageLines(mount).at(-1)).toBe("› Machine reset: hello is running");

    page.serial({
      stream: RingId.UsjTx,
      bytes: new TextEncoder().encode("I (12) hello: booted\n"),
      dropped: 0n,
      lines: [{ offset: 0n, vtPs: 12_000_000_000n }],
      linesDropped: 0n,
    });
    await settle();
    expect(pageLines(mount).at(-1)).toBe("› First console output");
    expect(mount.querySelector(".log-panel")?.textContent).toContain("I (12) hello: booted");
    expect(mount.querySelector("[data-loader]")?.getAttribute("data-loader-state")).toBe("loaded");
    expect(mount.querySelector(".sim-status [data-image]")?.getAttribute("data-image")).toBe("hello");
  });

  test("the ROM banner both serial streams print shows once in the log and tagged twice in the console", async () => {
    const { page, mount } = mountPage({ storage: () => memory() });
    const banner = (stream: number) =>
      page.serial({
        stream,
        bytes: new TextEncoder().encode("ESP-ROM:esp32c3-api1-20210207\n"),
        dropped: 0n,
        lines: [{ offset: 29n, vtPs: 1_000_000n }],
        linesDropped: 0n,
      });
    banner(RingId.Uart0Tx);
    banner(RingId.UsjTx);
    await settle();
    const rows = (scope: string) =>
      [...mount.querySelectorAll(`${scope} .terminal-line`)].filter((row) => row.textContent?.includes("ESP-ROM"));
    expect(rows(".log-panel").map((row) => row.getAttribute("data-stream"))).toEqual(["usj"]);
    expect(rows(".log-panel")[0]?.querySelector(".terminal-stream")).toBeNull();

    page.model.actions.setMode("advanced");
    await settle();
    expect(rows(".console").map((row) => row.querySelector(".terminal-stream")?.textContent?.trim())).toEqual(["uart0", "usj"]);
  });

  test("a refused file is an error on the firmware card, and the demo keeps running", async () => {
    const { page, mount, toWorker } = mountPage({ storage: () => memory() });
    await page.loader.offer({ root: null, files: [{ path: "notes.txt", size: 2, read: () => Promise.resolve(new Uint8Array([1, 2])) }] });
    await settle();
    const alert = mount.querySelector(".firmware-card [role=alert]");
    expect(alert?.textContent).toContain("This firmware could not be loaded");
    expect(alert?.textContent).toContain("is not a firmware image");
    expect(alert?.textContent).toContain("The previous firmware keeps running.");
    expect(toWorker).toEqual([]);
    expect(pageLines(mount).at(-1)).toBe("› Stopped");
  });

  test("with no demo served, the page waits for a firmware and says so, then runs the first drop", async () => {
    const { page, mount, toWorker } = mountPage({ storage: () => memory() });
    page.noDemo();
    await settle();
    const loaderState = () => mount.querySelector("[data-loader]")?.getAttribute("data-loader-state");
    expect(loaderState()).toBe("empty");
    expect(mount.querySelector("[data-glass-empty]")?.textContent).toBe("Drop firmware to start");
    expect(mount.querySelector(".sim-status [data-state]")?.getAttribute("data-state")).toBe("empty");
    expect(mount.querySelector(".sim-status")?.textContent).toContain("No firmware");
    expect(mount.querySelector(".firmware-card")?.textContent).toContain("Nothing yet");
    expect(mount.querySelector("[data-loader-message]")?.textContent).toContain("served without the demo firmware");
    expect(mount.querySelector('[data-action="back-to-demo"]')).toBeNull();
    expect(pageLines(mount)).toEqual(["› No demo firmware is served with this page; waiting for a firmware"]);
    // No machine has no clock or speed to state.
    expect(mount.querySelector(".sim-status [data-speed]")).toBeNull();
    expect(mount.querySelector(".sim-status")?.textContent).not.toContain("0.00x");
    for (const id of ["up", "ok", "down", "power"]) {
      expect((mount.querySelector(`[data-control=${id}]`) as HTMLButtonElement).disabled).toBe(true);
    }
    // Nothing ran, so there is nothing to start again.
    page.model.actions.restart();
    await settle();
    expect(toWorker).toEqual([]);

    // A refused drop does not claim a previous firmware keeps running.
    await page.loader.offer({ root: null, files: [{ path: "notes.txt", size: 2, read: () => Promise.resolve(new Uint8Array([1, 2])) }] });
    await settle();
    expect(mount.querySelector(".firmware-card [role=alert]")?.textContent).not.toContain("keeps running");
    expect(mount.querySelector("[data-glass-empty]")).not.toBeNull();

    const offered = page.loader.offer({
      root: null,
      files: [{ path: "hello.bin", size: 0x20_000, read: () => Promise.resolve(mergedBin()) }],
    });
    await settle();
    await settle();
    const boot = toWorker.find((message) => (message as { type?: string }).type === "boot") as { token: string };
    page.ready(boot.token);
    await offered;
    await settle();
    expect(loaderState()).toBe("loaded");
    expect(mount.querySelector("[data-glass-empty]")).toBeNull();
    expect(mount.querySelector(".sim-status [data-image]")?.getAttribute("data-image")).toBe("hello");
    for (const id of ["up", "ok", "down", "power"]) {
      expect((mount.querySelector(`[data-control=${id}]`) as HTMLButtonElement).disabled).toBe(false);
    }
    // Up, but not measured yet.
    expect(mount.querySelector(".sim-status [data-speed]")?.textContent).toBe("speed --x");
  });

  test("the header names the build `status` reports, by the digits the firmware prints, and nothing it did not", async () => {
    const sha = "7d711d0b5".padEnd(64, "0");
    let build: unknown = { elf_sha256: sha };
    const asked: string[] = [];
    const { page, toWorker } = mountPage({
      storage: () => memory(),
      transport: (request) => {
        const name = (JSON.parse(request) as { cmd: string }).cmd;
        asked.push(name);
        const json = name === "status" ? { instances: [{ instance: "b1", build }] } : {};
        return Promise.resolve({ ok: JSON.stringify({ json, text: "" }) });
      },
    });
    expect(page.model.header.get().buildId).toBe("--");
    const load = async () => {
      const offered = page.loader.offer({
        root: null,
        files: [{ path: "hello.bin", size: 0x20_000, read: () => Promise.resolve(mergedBin()) }],
      });
      await settle();
      await settle();
      const boot = toWorker.filter((message) => (message as { type?: string }).type === "boot").at(-1) as { token: string };
      // Until the new machine answers, the old build is not claimed for it.
      expect(page.model.header.get().buildId).toBe("--");
      page.ready(boot.token);
      await offered;
      await settle();
    };
    await load();
    expect(asked).toContain("status");
    expect(page.model.header.get().buildId).toBe("7d711d0b5");

    build = null;
    await load();
    expect(page.model.header.get().buildId).toBe("--");
  });

  test("a boot the Worker refuses is an error too, but it does not claim the old firmware runs on", async () => {
    // By then the machine was already replaced, so "keeps running" would be false.
    const { page, mount } = mountPage({ storage: () => memory() });
    const offered = page.loader.offer({
      root: null,
      files: [{ path: "hello.bin", size: 0x20_000, read: () => Promise.resolve(mergedBin()) }],
    });
    await settle();
    await settle();
    page.workerError("E_ASSET_MISSING: the core refused the assets");
    await offered;
    await settle();
    const alert = mount.querySelector(".firmware-card [role=alert]");
    expect(alert?.textContent).toContain("E_ASSET_MISSING");
    expect(alert?.textContent).not.toContain("keeps running");
  });
});
