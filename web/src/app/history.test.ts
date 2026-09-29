// The IndexedDB backend itself is exercised in a browser by `tests/history.spec.ts`.

import { describe, expect, test } from "bun:test";
import { LoadKind } from "../worker/layout";
import {
  CARDID_WINDOW,
  FirmwareHistory,
  MAX_ENTRIES,
  MAX_TOTAL_BYTES,
  buildFieldsOf,
  carriesCardId,
  filesDigest,
  memoryBackend,
  type HistoryBackend,
} from "./history";
import { IMAGE_MAGIC, type LoadedImage } from "./load";

function image(name: string, fill: number, length = 0x1000, kind: LoadKind = LoadKind.MergedFlash): LoadedImage {
  const bytes = new Uint8Array(length).fill(fill);
  bytes[0] = IMAGE_MAGIC;
  return { name, assets: [{ kind, bytes, file: `${name}.bin` }], notes: [] };
}

function clock(start = 1_000) {
  let now = start;
  return { now: () => now, advance: (ms = 1) => (now += ms) };
}

async function opened(backend: HistoryBackend = memoryBackend(), now = clock()) {
  const history = new FirmwareHistory(() => Promise.resolve(backend), now.now);
  await history.ready();
  return { history, backend, now };
}

describe("what is kept", () => {
  test("a booted image is kept with its files, size and names; nothing about it is known yet", async () => {
    const { history, backend } = await opened();
    const id = await history.record(image("pk", 1));
    expect(id).not.toBeNull();
    const [entry] = history.store.get().entries;
    expect(entry).toMatchObject({ id, name: "pk", size: 0x1000, buildId: null, project: null, version: null, thumbnail: null });
    expect(entry?.files).toEqual([{ name: "pk.bin", kind: LoadKind.MergedFlash, size: 0x1000 }]);
    expect((await backend.files(id ?? ""))?.[0]?.bytes.length).toBe(0x1000);
  });

  test("the same firmware loaded again is one entry, moved to the top", async () => {
    const { history, now } = await opened();
    const first = await history.record(image("a", 1));
    now.advance();
    await history.record(image("b", 2));
    now.advance();
    const again = await history.record(image("a", 1));
    expect(again).toBe(first);
    const entries = history.store.get().entries;
    expect(entries.map((entry) => entry.name)).toEqual(["a", "b"]);
    expect(entries[0]?.firstLoadedAt).toBeLessThan(entries[0]?.lastLoadedAt ?? 0);
  });

  test("a merged image with anything in the cardid window is a device backup and is not kept", async () => {
    const { history } = await opened();
    const backup = image("backup", 0xff, CARDID_WINDOW.end);
    const bytes = backup.assets[0]?.bytes as Uint8Array;
    expect(carriesCardId(bytes)).toBe(false);
    bytes[CARDID_WINDOW.start + 7] = 0x42;
    expect(carriesCardId(bytes)).toBe(true);
    expect(await history.record(backup)).toBeNull();
    expect(history.store.get().entries).toEqual([]);
    expect(history.store.get().notKept).toEqual({ kind: "device-backup", name: "backup" });
    // An ELF is never a flash image, whatever its bytes.
    expect(await history.record(image("elf", 0x42, CARDID_WINDOW.end, LoadKind.AppElf))).not.toBeNull();
  });

  test("a firmware larger than the whole budget is not kept, and says so", async () => {
    const { history } = await opened();
    const big: LoadedImage = { name: "big", assets: [{ kind: LoadKind.AppElf, bytes: new Uint8Array(MAX_TOTAL_BYTES + 1) }], notes: [] };
    expect(await history.record(big)).toBeNull();
    expect(history.store.get().notKept).toMatchObject({ kind: "too-large", name: "big" });
  });
});

describe("the limits", () => {
  test(`at ${MAX_ENTRIES} entries the one loaded longest ago goes first`, async () => {
    const { history, backend, now } = await opened();
    const ids: (string | null)[] = [];
    for (let i = 0; i < MAX_ENTRIES + 2; i += 1) {
      now.advance();
      ids.push(await history.record(image(`fw${i}`, i + 1)));
    }
    const names = history.store.get().entries.map((entry) => entry.name);
    expect(names.length).toBe(MAX_ENTRIES);
    expect(names).not.toContain("fw0");
    expect(names).not.toContain("fw1");
    expect(names[0]).toBe(`fw${MAX_ENTRIES + 1}`);
    expect(await backend.files(ids[0] ?? "")).toBeNull();
  });

  test("the byte budget evicts the oldest until the new one fits", async () => {
    const { history, now } = await opened();
    const third = Math.floor(MAX_TOTAL_BYTES / 3);
    for (const [name, fill] of [
      ["one", 1],
      ["two", 2],
      ["three", 3],
    ] as const) {
      now.advance();
      await history.record({ name, assets: [{ kind: LoadKind.AppElf, bytes: new Uint8Array(third).fill(fill) }], notes: [] });
    }
    now.advance();
    await history.record({ name: "four", assets: [{ kind: LoadKind.AppElf, bytes: new Uint8Array(third).fill(4) }], notes: [] });
    expect(history.store.get().entries.map((entry) => entry.name)).toEqual(["four", "three", "two"]);
  });

  test("a write refused for quota evicts one more and retries; with nothing left it is not kept", async () => {
    const inner = memoryBackend();
    let refusals = 0;
    const quota = Object.assign(new Error("full"), { name: "QuotaExceededError" });
    const backend: HistoryBackend = {
      ...inner,
      put: (entry, files) => (files !== null && refusals-- > 0 ? Promise.reject(quota) : inner.put(entry, files)),
    };
    const { history, now } = await opened(backend);
    await history.record(image("old", 1));
    refusals = 1;
    now.advance();
    expect(await history.record(image("new", 2))).not.toBeNull();
    expect(history.store.get().entries.map((entry) => entry.name)).toEqual(["new"]);

    refusals = 5;
    now.advance();
    expect(await history.record(image("newer", 3))).toBeNull();
    expect(history.store.get().notKept).toMatchObject({ kind: "quota", name: "newer" });
    expect(history.store.get().entries).toEqual([]);
  });
});

describe("storage that fails", () => {
  test("a backend that will not open leaves the history unavailable, with the browser's reason", async () => {
    const history = new FirmwareHistory(() => Promise.reject(new DOMException("The operation is insecure.", "SecurityError")));
    await history.ready();
    expect(history.store.get()).toMatchObject({ state: "unavailable", reason: "SecurityError: The operation is insecure." });
    expect(await history.record(image("pk", 1))).toBeNull();
  });

  test("no backend at all is unavailable too", async () => {
    const history = new FirmwareHistory(null);
    await history.ready();
    expect(history.store.get().state).toBe("unavailable");
  });

  test("a write that fails for another reason is stated, and the list is unchanged", async () => {
    const inner = memoryBackend();
    const backend: HistoryBackend = { ...inner, put: () => Promise.reject(new Error("disk on fire")) };
    const { history } = await opened(backend);
    expect(await history.record(image("pk", 1))).toBeNull();
    expect(history.store.get()).toMatchObject({ entries: [], error: "Error: disk on fire" });
  });
});

describe("after the boot", () => {
  test("the build fields and the picture are added to the entry", async () => {
    const { history, backend } = await opened();
    const id = (await history.record(image("pk", 1))) ?? "";
    const status = { instances: [{ build: { project: "FoloToy-AI-Passport", version: "1.0.0", elf_sha256: "ab".repeat(32) } }] };
    await history.annotate(id, { buildId: "abababab0", ...buildFieldsOf(status) });
    await history.annotate(id, { thumbnail: new Uint8Array([0x89, 0x50]) });
    expect(history.store.get().entries[0]).toMatchObject({ buildId: "abababab0", project: "FoloToy-AI-Passport", version: "1.0.0" });
    expect((await backend.list())[0]?.thumbnail).toEqual(new Uint8Array([0x89, 0x50]));
    expect(buildFieldsOf({ instances: [{ build: null }] })).toEqual({ project: null, version: null });
  });

  test("an entry comes back as the image it booted as, files named as they were dropped", async () => {
    const { history } = await opened();
    const original = image("pk", 7);
    const id = (await history.record(original)) ?? "";
    const again = await history.image(id);
    expect(again?.name).toBe("pk");
    expect(again?.assets).toEqual([{ kind: LoadKind.MergedFlash, bytes: original.assets[0]?.bytes as Uint8Array, file: "pk.bin" }]);
  });

  test("delete removes one entry, clear removes all", async () => {
    const { history, backend, now } = await opened();
    const a = (await history.record(image("a", 1))) ?? "";
    now.advance();
    await history.record(image("b", 2));
    await history.remove(a);
    expect(history.store.get().entries.map((entry) => entry.name)).toEqual(["b"]);
    expect(await backend.files(a)).toBeNull();
    await history.clear();
    expect(history.store.get().entries).toEqual([]);
    expect(await backend.list()).toEqual([]);
  });

  test("the digest tells files apart by content and by kind", () => {
    const bytes = new Uint8Array([1, 2, 3]);
    expect(filesDigest([{ kind: 1, bytes }])).toBe(filesDigest([{ kind: 1, bytes: new Uint8Array([1, 2, 3]) }]));
    expect(filesDigest([{ kind: 1, bytes }])).not.toBe(filesDigest([{ kind: 2, bytes }]));
    expect(filesDigest([{ kind: 1, bytes }])).not.toBe(filesDigest([{ kind: 1, bytes: new Uint8Array([1, 2, 4]) }]));
  });
});
