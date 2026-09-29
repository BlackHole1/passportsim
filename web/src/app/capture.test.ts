import { describe, expect, test } from "bun:test";
import type { PacingStats } from "../worker/pacing";
import type { PanelState } from "../worker/session";
import { frameRgba, LIT_PANEL, screenshotName, type RawFrame } from "./capture";
import { memoryBackend } from "./history";
import { createPage, THUMBNAIL_VT_PS } from "./page";
import { initialFollow, PREF_KEYS, safeStorage } from "./prefs";
import { masonrySpan, MASONRY_GAP_PX, MASONRY_ROW_PX } from "./view/Workbench";
import { installDom, settle } from "./view/testDom";

const window = installDom();
const document = window.document as unknown as Document;

function frame(pixel: number): RawFrame {
  return { width: 240, height: 320, pixels: new Uint16Array(240 * 320).fill(pixel) };
}

const RED = 0xf800;

const PANEL: PanelState = {
  backlight: 1,
  backlightScale: 1,
  powered: true,
  sleeping: false,
  displayOn: true,
  inverted: false,
  glassComplement: false,
};

function stats(nowPs: bigint): PacingStats {
  return { mode: "Wall", nowPs, realTimeFactor: 1, reanchors: 0, sliceVtPs: 0n } as PacingStats;
}

const STATUS = {
  instances: [{ fw: "hello", build: { project: "hello-project", version: "2.1.0", idf_ver: "v5.5.3", elf_sha256: "0123456789".repeat(6) + "abcd" } }],
};

function mount() {
  const root = document.createElement("div");
  document.body.appendChild(root);
  const toWorker: unknown[] = [];
  const downloads: { name: string; blob: Blob }[] = [];
  const encoded: { rgba: Uint8ClampedArray; width: number; height: number; scale: number }[] = [];
  const backend = memoryBackend();
  let current: RawFrame | null = frame(RED);
  let clock = 0;
  const page = createPage({
    mount: root,
    transport: (request: string) => {
      const command = (JSON.parse(request) as { cmd?: string }).cmd;
      return Promise.resolve({ ok: JSON.stringify({ json: command === "status" ? STATUS : {}, text: "" }) });
    },
    toWorker: (message) => toWorker.push(message),
    rewindSource: { snapshot: () => new Uint8Array(0), frame: () => null },
    now: () => (clock += 3),
    confirm: () => true,
    viewport: () => ({ width: 1440, height: 900 }),
    devicePixelRatio: () => 1,
    readFrame: () => Promise.resolve(current),
    encodePng: (rgba, width, height, scale) => {
      encoded.push({ rgba, width, height, scale });
      return Promise.resolve(new Blob([new Uint8Array([0x89, 0x50, 0x4e, 0x47, scale === 1 ? 1 : 2])], { type: "image/png" }));
    },
    download: (blob, name) => downloads.push({ blob, name }),
    history: () => Promise.resolve(backend),
    wallClock: () => 1_700_000_000_000,
  });
  return {
    page,
    root,
    toWorker,
    downloads,
    encoded,
    backend,
    setFrame: (next: RawFrame | null) => {
      current = next;
    },
  };
}

function mergedBin(): Uint8Array {
  const bytes = new Uint8Array(0x20_000);
  bytes[0] = 0xe9;
  return bytes;
}

async function dropAndBoot(mounted: ReturnType<typeof mount>): Promise<void> {
  const offered = mounted.page.loader.offer({ root: null, files: [{ path: "hello.bin", size: 0x20_000, read: () => Promise.resolve(mergedBin()) }] });
  for (let i = 0; i < 4; i += 1) {
    await settle();
  }
  const boot = mounted.toWorker.filter((message) => (message as { type?: string }).type === "boot").at(-1) as { token: string };
  mounted.page.ready(boot.token);
  await offered;
  for (let i = 0; i < 4; i += 1) {
    await settle();
  }
}

describe("the screenshot", () => {
  test("is the frame as the glass shows it, downloaded as <image>-vt<time>.png", async () => {
    const mounted = mount();
    mounted.page.stats(stats(6_580_000_000_000n));
    await mounted.page.model.actions.screenshot();
    expect(mounted.downloads.map((one) => one.name)).toEqual(["official-vt6.580s.png"]);
    const [shot] = mounted.encoded;
    expect([shot?.width, shot?.height, shot?.scale]).toEqual([240, 320, 1]);
    expect([...(shot?.rgba.subarray(0, 4) ?? [])]).toEqual([255, 0, 0, 255]);
    expect(mounted.page.model.capture.get()).toEqual({ kind: "saved", file: "official-vt6.580s.png" });

    // Through the panel state: a dark rail is a black picture, as the glass is.
    mounted.page.panel({ ...PANEL, powered: false });
    await mounted.page.model.actions.screenshot();
    expect([...(mounted.encoded[1]?.rgba.subarray(0, 4) ?? [])]).toEqual([0, 0, 0, 255]);
  });

  test("with no frame to read it downloads nothing and says why", async () => {
    const mounted = mount();
    mounted.setFrame(null);
    await mounted.page.model.actions.screenshot();
    expect(mounted.downloads).toEqual([]);
    expect(mounted.page.model.capture.get()).toMatchObject({ kind: "failed" });
  });

  test("helpers: the name keeps only safe characters; a short frame has no picture", () => {
    expect(screenshotName("My Build (v2)", 1_234_000_000_000n)).toBe("My_Build_v2_-vt1.234s.png");
    expect(screenshotName("../..", 0n)).toBe("passportsim-vt0.000s.png");
    expect(frameRgba({ width: 240, height: 320, pixels: new Uint16Array(10) }, LIT_PANEL)).toBeNull();
  });
});

describe("the history, mounted", () => {
  test("a dropped firmware that boots is kept with its build, and its picture once vt passes the mark", async () => {
    const mounted = mount();
    await mounted.page.model.history.ready();
    await dropAndBoot(mounted);
    const [entry] = [...mounted.backend.entries.values()];
    expect(entry).toMatchObject({ name: "hello", buildId: "012345678", project: "hello-project", version: "2.1.0", thumbnail: null });
    expect(entry?.files).toEqual([{ name: "hello.bin", kind: 1, size: 0x20_000 }]);

    // Not before the boot has settled in virtual time.
    mounted.page.stats(stats(THUMBNAIL_VT_PS - 1n));
    await settle();
    expect(mounted.encoded).toEqual([]);
    mounted.page.stats(stats(THUMBNAIL_VT_PS));
    for (let i = 0; i < 4; i += 1) {
      await settle();
    }
    expect(mounted.encoded.map((one) => one.scale)).toEqual([0.5]);
    expect([...(mounted.backend.entries.get(entry?.id ?? "")?.thumbnail ?? [])]).toEqual([0x89, 0x50, 0x4e, 0x47, 2]);
    // Once only.
    mounted.page.stats(stats(THUMBNAIL_VT_PS * 2n));
    await settle();
    expect(mounted.encoded.length).toBe(1);
  });

  test("a refused drop is not kept, and the demo the page opens with is not either", async () => {
    const mounted = mount();
    await mounted.page.model.history.ready();
    mounted.page.ready("demo");
    mounted.page.stats(stats(THUMBNAIL_VT_PS * 2n));
    await mounted.page.loader.offer({ root: null, files: [{ path: "notes.txt", size: 2, read: () => Promise.resolve(new Uint8Array([1, 2])) }] });
    await settle();
    expect(mounted.backend.entries.size).toBe(0);
    const back = mounted.page.loader.backToDemo();
    for (let i = 0; i < 4; i += 1) {
      await settle();
    }
    const boot = mounted.toWorker.filter((message) => (message as { type?: string }).type === "boot").at(-1) as { token: string };
    mounted.page.ready(boot.token);
    await back;
    mounted.page.stats(stats(THUMBNAIL_VT_PS * 2n));
    await settle();
    expect(mounted.page.loader.store.get().image).toBe("official");
    expect(mounted.backend.entries.size).toBe(0);
    expect(mounted.encoded).toEqual([]);
  });

  test("load again boots the kept files and says where they came from", async () => {
    const mounted = mount();
    await mounted.page.model.history.ready();
    await dropAndBoot(mounted);
    const id = [...mounted.backend.entries.keys()][0] ?? "";
    const loading = mounted.page.model.actions.loadFromHistory(id);
    for (let i = 0; i < 4; i += 1) {
      await settle();
    }
    const boots = mounted.toWorker.filter((message) => (message as { type?: string }).type === "boot") as { token: string; assets?: { kind: number }[] }[];
    expect(boots.at(-1)?.assets?.map((asset) => asset.kind)).toEqual([1]);
    mounted.page.ready(boots.at(-1)?.token);
    await loading;
    await settle();
    expect(mounted.page.loader.store.get().steps[0]?.step).toEqual({ kind: "history", name: "hello" });
    expect(mounted.page.loader.store.get().image).toBe("hello");
    expect(mounted.backend.entries.size).toBe(1);

    await mounted.page.model.actions.downloadFromHistory(id);
    expect(mounted.downloads.map((one) => [one.name, one.blob.size])).toEqual([["hello.bin", 0x20_000]]);
  });
});

describe("the follow-latest preference", () => {
  test("is on unless it was turned off, and the switch is stored", () => {
    const map = new Map<string, string>();
    const storage = safeStorage(() => ({ getItem: (key) => map.get(key) ?? null, setItem: (key, value) => void map.set(key, value) }));
    expect(initialFollow(storage)).toBe(true);
    const mounted = createPage({
      mount: document.body.appendChild(document.createElement("div")),
      transport: () => Promise.resolve({ ok: JSON.stringify({ json: {}, text: "" }) }),
      toWorker: () => undefined,
      rewindSource: { snapshot: () => new Uint8Array(0), frame: () => null },
      now: () => 0,
      confirm: () => true,
      viewport: () => ({ width: 1440, height: 900 }),
      devicePixelRatio: () => 1,
      storage: () => ({ getItem: (key) => map.get(key) ?? null, setItem: (key, value) => void map.set(key, value) }),
    });
    mounted.model.actions.setFollow(false);
    expect(map.get(PREF_KEYS.follow)).toBe("false");
    expect(initialFollow(storage)).toBe(false);
  });
});

describe("the cards' masonry", () => {
  test("a card spans its height and the gap under it, in whole rows", () => {
    expect(masonrySpan(0)).toBe(MASONRY_GAP_PX / MASONRY_ROW_PX);
    expect(masonrySpan(100)).toBe((100 + MASONRY_GAP_PX) / MASONRY_ROW_PX);
    expect(masonrySpan(101)).toBe(Math.ceil((101 + MASONRY_GAP_PX) / MASONRY_ROW_PX));
  });
});
