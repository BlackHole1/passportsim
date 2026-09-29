// The `File`s are plain objects cast to `File`: the loader reads only `name`, `size`,
// `webkitRelativePath` and `arrayBuffer`.

import { describe, expect, test } from "bun:test";
import { LoadKind } from "../worker/layout";
import { dropFromFiles, dropFromTransfer } from "./drop";
import { translator } from "./i18n";
import { IMAGE_MAGIC, type LoadedImage } from "./load";
import { createLoader } from "./loader";
import { stepText } from "./view/Log";
import { loaderText } from "./view/Firmware";
import { installDom } from "./view/testDom";

const window = installDom();
const document = window.document as unknown as Document;
const t = translator("en");

function mergedBin(length = 0x20_000): Uint8Array {
  const bytes = new Uint8Array(length);
  bytes[0] = IMAGE_MAGIC;
  return bytes;
}

function fakeFile(name: string, bytes: Uint8Array, relative = ""): File {
  return {
    name,
    size: bytes.length,
    webkitRelativePath: relative,
    arrayBuffer: () => Promise.resolve(bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength)),
  } as unknown as File;
}

function fileEntry(name: string, bytes: Uint8Array): FileSystemEntry {
  return {
    isFile: true,
    isDirectory: false,
    name,
    file: (resolve: (file: File) => void) => {
      resolve(fakeFile(name, bytes));
    },
  } as unknown as FileSystemEntry;
}

function dirEntry(name: string, children: FileSystemEntry[]): FileSystemEntry {
  return {
    isFile: false,
    isDirectory: true,
    name,
    createReader: () => {
      let done = false;
      return {
        readEntries: (resolve: (entries: FileSystemEntry[]) => void) => {
          const batch = done ? [] : children;
          done = true;
          resolve(batch);
        },
      };
    },
  } as unknown as FileSystemEntry;
}

function item(entry: FileSystemEntry | null): DataTransferItem {
  return { webkitGetAsEntry: () => entry } as unknown as DataTransferItem;
}

function loaderFor(onImage: (image: LoadedImage) => Promise<void> = () => Promise.resolve()) {
  const seen: LoadedImage[] = [];
  let clock = 1_000;
  const loader = createLoader(
    {
      onImage: async (image) => {
        seen.push(image);
        clock += 5;
        await onImage(image);
      },
      onDemo: () => Promise.resolve(),
      now: () => clock,
    },
    "official",
  );
  return {
    loader,
    seen,
    state: () => loader.store.get().state,
    image: () => loader.store.get().image,
    message: () => loaderText(t, loader.store.get().message),
    steps: () => loader.store.get().steps,
  };
}

describe("dropFromFiles (the file inputs)", () => {
  test("a `webkitdirectory` input gives the picked directory as the drop's root", () => {
    const drop = dropFromFiles([
      fakeFile("flash_args", new Uint8Array(1), "build/flash_args"),
      fakeFile("bootloader.bin", new Uint8Array(1), "build/bootloader/bootloader.bin"),
    ]);
    expect(drop.root).toBe("build");
    expect(drop.files.map((file) => file.path)).toEqual(["flash_args", "bootloader/bootloader.bin"]);
  });

  test("a plain input gives a rootless drop of bare names", () => {
    const drop = dropFromFiles([fakeFile("a.bin", new Uint8Array(1)), fakeFile("a.elf", new Uint8Array(1))]);
    expect(drop.root).toBeNull();
    expect(drop.files.map((file) => file.path)).toEqual(["a.bin", "a.elf"]);
  });
});

describe("dropFromTransfer (the drag)", () => {
  test("walks a dragged directory into paths relative to it", async () => {
    const transfer = {
      items: [item(dirEntry("build", [fileEntry("app.bin", mergedBin()), dirEntry("bootloader", [fileEntry("bootloader.bin", new Uint8Array(2))])]))],
      files: [],
    };
    const drop = await dropFromTransfer(transfer, 100);
    expect(drop.root).toBe("build");
    expect(drop.files.map((file) => file.path).sort()).toEqual(["app.bin", "bootloader/bootloader.bin"]);
  });

  test("falls back to `files` when the items answer no entry, which is every synthesized transfer", async () => {
    const drop = await dropFromTransfer({ items: [item(null)], files: [fakeFile("a.bin", mergedBin())] }, 100);
    expect(drop.root).toBeNull();
    expect(drop.files.map((file) => file.path)).toEqual(["a.bin"]);
  });

  test("falls back to `files` when the entry API answers an entry it cannot open (WebKit, synthesized)", async () => {
    const broken = {
      isFile: true,
      isDirectory: false,
      name: "a.bin",
      file: (_ok: unknown, fail: (error: Error) => void) => {
        fail(new Error("Path does not exist"));
      },
    } as unknown as FileSystemEntry;
    const drop = await dropFromTransfer({ items: [item(broken)], files: [fakeFile("a.bin", mergedBin())] }, 100);
    expect(drop.files.map((file) => file.path)).toEqual(["a.bin"]);
  });

  test("a directory whose walk fails after a file reports the error rather than half the drop", async () => {
    const broken = {
      isFile: true,
      isDirectory: false,
      name: "b.bin",
      file: (_ok: unknown, fail: (error: Error) => void) => {
        fail(new Error("NotReadableError"));
      },
    } as unknown as FileSystemEntry;
    const dir = dirEntry("build", [fileEntry("a.bin", mergedBin()), broken]);
    await expect(dropFromTransfer({ items: [item(dir)], files: [] }, 100)).rejects.toThrow("NotReadableError");
  });

  test("stops walking at the limit, so a dropped home directory cannot fill the tab", async () => {
    const many = Array.from({ length: 20 }, (_, i) => fileEntry(`f${i}.bin`, new Uint8Array(1)));
    const drop = await dropFromTransfer({ items: [item(dirEntry("many", many))], files: [] }, 5);
    expect(drop.files.length).toBe(5);
  });
});

describe("the loader states what it did", () => {
  test("a loaded image names itself, its files and its roles", async () => {
    const loader = loaderFor();
    await loader.loader.offer({
      root: "pk",
      files: [
        { path: "FoloToy-AI-Passport-8MB.bin", size: 0x20_000, read: () => Promise.resolve(mergedBin()) },
        { path: "FoloToy-AI-Passport.elf", size: 4, read: () => Promise.resolve(new Uint8Array([0x7f, 0x45, 0x4c, 0x46])) },
      ],
    });
    expect(loader.state()).toBe("loaded");
    expect(loader.image()).toBe("pk");
    expect(loader.message()).toContain("running pk");
    expect(loader.seen[0]?.assets.map((asset) => asset.kind)).toEqual([LoadKind.MergedFlash, LoadKind.AppElf]);
  });

  test("a refusal is a page message and boots nothing", async () => {
    const loader = loaderFor();
    await loader.loader.offer({ root: null, files: [{ path: "notes.txt", size: 2, read: () => Promise.resolve(new Uint8Array([1, 2])) }] });
    expect(loader.state()).toBe("refused");
    expect(loader.message()).toContain("is not a firmware image");
    expect(loader.seen).toEqual([]);
  });

  test("a file the browser cannot read is the same page message, not a throw", async () => {
    const loader = loaderFor();
    await loader.loader.offer({
      root: null,
      files: [{ path: "a.bin", size: 4, read: () => Promise.reject(new Error("NotReadableError")) }],
    });
    expect(loader.state()).toBe("refused");
    expect(loader.message()).toContain("NotReadableError");
  });

  test("a boot the Worker refuses is stated too, and the header keeps the old image", async () => {
    const loader = loaderFor(() => Promise.reject(new Error("E_ASSET_MISSING: no firmware")));
    await loader.loader.offer({ root: null, files: [{ path: "a.bin", size: 0x20_000, read: () => Promise.resolve(mergedBin()) }] });
    expect(loader.state()).toBe("error");
    expect(loader.message()).toContain("E_ASSET_MISSING");
    expect(loader.image()).toBe("official");
  });

  test("`say` shows what the page decided, such as a Worker error", () => {
    const loader = loaderFor();
    loader.loader.say("error", { kind: "machine", detail: "the firmware bundle for `official` is not served (404)" });
    expect(loader.state()).toBe("error");
    expect(loader.message()).toContain("404");
  });
});

describe("the drop zone", () => {
  test("a drop with no file is left alone: the NFC zone's dropped NDEF text bubbles here", async () => {
    const loader = loaderFor();
    const zone = document.createElement("div");
    loader.loader.watchDrops(zone);
    const event = new window.Event("drop", { bubbles: true, cancelable: true }) as unknown as Event;
    (event as unknown as { dataTransfer: unknown }).dataTransfer = { items: [], files: [] };
    zone.dispatchEvent(event);
    await Promise.resolve();
    expect(loader.state()).toBe("idle");
  });

  test("a dragover is taken, so the browser does not navigate to the dropped file", () => {
    const loader = loaderFor();
    const zone = document.createElement("div");
    loader.loader.watchDrops(zone);
    const event = new window.Event("dragover", { bubbles: true, cancelable: true }) as unknown as Event;
    zone.dispatchEvent(event);
    expect(event.defaultPrevented).toBe(true);
    expect(loader.loader.store.get().over).toBe(true);
  });
});

describe("the progress lines (simple mode's log)", () => {
  test("a merged image and its ELF: received, detected, read, each once and in order", async () => {
    const loader = loaderFor();
    await loader.loader.offer({
      root: null,
      files: [
        { path: "app-8MB.bin", size: 0x20_000, read: () => Promise.resolve(mergedBin()) },
        { path: "app.elf", size: 4, read: () => Promise.resolve(new Uint8Array([0x7f, 0x45, 0x4c, 0x46])) },
      ],
    });
    const steps = loader.steps();
    expect(steps.map((line) => line.step.kind)).toEqual(["received", "detected", "read", "read"]);
    expect(steps.map((line) => stepText(t, line.step))).toEqual([
      "Received 2 file(s), 128.0 KiB in total",
      "Reading the dropped files one by one",
      "Read app-8MB.bin (128.0 KiB)",
      "Read app.elf (4 B)",
    ]);
    // Stamped from the start of this load, not from page load.
    expect(steps[0]?.atMs).toBe(0);
  });

  test("the page's own steps follow, and a new load starts a new set", async () => {
    const loader = loaderFor();
    loader.loader.begin({ kind: "demo" });
    loader.loader.progress({ kind: "ready", name: "official" });
    expect(loader.steps().map((line) => line.step.kind)).toEqual(["demo", "ready"]);

    await loader.loader.offer({ root: null, files: [{ path: "a.bin", size: 0x20_000, read: () => Promise.resolve(mergedBin()) }] });
    loader.loader.progress({ kind: "boot", name: "a", assets: 1 });
    loader.loader.progress({ kind: "ready", name: "a" });
    loader.loader.progress({ kind: "console" });
    const kinds = loader.steps().map((line) => line.step.kind);
    expect(kinds[0]).toBe("received");
    expect(kinds).not.toContain("demo");
    expect(kinds.slice(-3)).toEqual(["boot", "ready", "console"]);
    expect(stepText(t, { kind: "ready", name: "a" })).toBe("Machine reset: a is running");
  });

  test("a refused drop ends its lines with a stop, and says why", async () => {
    const loader = loaderFor();
    await loader.loader.offer({ root: null, files: [{ path: "notes.txt", size: 2, read: () => Promise.resolve(new Uint8Array([1, 2])) }] });
    expect(loader.steps().at(-1)?.step.kind).toBe("failed");
    expect(loader.message()).toContain("is not a firmware image");
  });

  test("the lines are worded in the page's language, with names left as they are", () => {
    const zh = translator("zh-CN");
    const step = { kind: "read", path: "build/app.bin", bytes: 2048 } as const;
    expect(stepText(zh, step)).toContain("build/app.bin");
    expect(stepText(zh, step)).not.toBe(stepText(t, step));
  });
});
