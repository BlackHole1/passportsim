// Includes the backslash `flash_files` fixture: written with `\` it loads, and its absolute and
// `..` forms are refused.

import { describe, expect, test } from "bun:test";
import { LoadKind } from "../worker/layout";
import {
  BUNDLE_MAGIC,
  FLASH_SIZE_BYTES,
  IMAGE_MAGIC,
  MAX_FILE_BYTES,
  assembleMerged,
  bundleCarriesAppElf,
  bundleFault,
  carriesAppElf,
  humanSize,
  imageName,
  loadDrop,
  parseFlashFiles,
  relativePath,
  type Drop,
  type DropFile,
} from "./load";

function file(path: string, bytes: Uint8Array): DropFile {
  return { path, size: bytes.length, read: () => Promise.resolve(bytes) };
}

function text(path: string, body: string): DropFile {
  return file(path, new TextEncoder().encode(body));
}

function mergedBin(length = 0x20_000): Uint8Array {
  const bytes = new Uint8Array(length).fill(0);
  bytes[0] = IMAGE_MAGIC;
  return bytes;
}

function elf(length = 64): Uint8Array {
  const bytes = new Uint8Array(length).fill(0);
  bytes.set([0x7f, 0x45, 0x4c, 0x46], 0);
  return bytes;
}

function bundle(manifest = '[bundle]\nid = "pk"\n'): Uint8Array {
  const body = new TextEncoder().encode(manifest);
  const bytes = new Uint8Array(12 + body.length);
  bytes.set(new TextEncoder().encode(BUNDLE_MAGIC), 0);
  new DataView(bytes.buffer).setUint32(8, body.length, true);
  bytes.set(body, 12);
  return bytes;
}

async function kinds(drop: Drop): Promise<LoadKind[]> {
  const result = await loadDrop(drop);
  if (!result.ok) {
    throw new Error(`expected a load, got: ${result.reason}`);
  }
  return result.image.assets.map((asset) => asset.kind);
}

async function refusal(drop: Drop): Promise<string> {
  const result = await loadDrop(drop);
  if (result.ok) {
    throw new Error(`expected a refusal, got the image \`${result.image.name}\``);
  }
  return result.reason;
}

describe("relativePath (relative, both separators, no absolute path and no `..`)", () => {
  test("splits on both separators", () => {
    expect(relativePath("bootloader\\bootloader.bin")).toEqual({ ok: true, path: "bootloader/bootloader.bin" });
    expect(relativePath("bootloader/bootloader.bin")).toEqual({ ok: true, path: "bootloader/bootloader.bin" });
    expect(relativePath(".\\app.bin")).toEqual({ ok: true, path: "app.bin" });
  });

  test("refuses an absolute path, a drive and `..`, in either spelling", () => {
    for (const value of ["/etc/passwd", "\\\\host\\share\\app.bin", "C:\\build\\app.bin", "../../etc/passwd", "a\\..\\..\\b.bin"]) {
      const out = relativePath(value);
      expect(out.ok, value).toBe(false);
    }
  });

  test("refuses a value that names no file", () => {
    expect(relativePath("   ").ok).toBe(false);
    expect(relativePath("./.").ok).toBe(false);
  });
});

describe("parseFlashFiles", () => {
  test("reads the `flash_files` of a flasher_args.json", () => {
    const out = parseFlashFiles(
      "flasher_args.json",
      JSON.stringify({ flash_files: { "0x10000": "app.bin", "0x0": "bootloader/bootloader.bin" } }),
    );
    expect(out).toEqual({
      ok: true,
      entries: [
        { offset: 0, value: "bootloader/bootloader.bin" },
        { offset: 0x10000, value: "app.bin" },
      ],
    });
  });

  test("reads the offset-and-file pairs of a flash_args, options and all", () => {
    const out = parseFlashFiles(
      "flash_args",
      "--flash_mode dio --flash_freq 80m --flash_size 8MB\n0x0 bootloader/bootloader.bin\n0x8000 partition_table/partition-table.bin\n0x10000 app.bin\n",
    );
    expect(out.ok && out.entries.map((entry) => entry.value)).toEqual([
      "bootloader/bootloader.bin",
      "partition_table/partition-table.bin",
      "app.bin",
    ]);
  });

  test("says so when there is nothing to read", () => {
    expect(parseFlashFiles("flasher_args.json", "{")).toMatchObject({ ok: false });
    expect(parseFlashFiles("flasher_args.json", "{}")).toMatchObject({ ok: false });
    expect(parseFlashFiles("flash_args", "--flash_mode dio\n")).toMatchObject({ ok: false });
  });
});

describe("assembleMerged", () => {
  test("places each part at its offset and pads the rest with the erased byte", () => {
    const parts = new Map([["app.bin", new Uint8Array([1, 2, 3])]]);
    const out = assembleMerged("flash_args", [{ offset: 0x10, value: "app.bin" }], parts);
    expect(out.ok).toBe(true);
    if (!out.ok) {
      return;
    }
    expect(out.bytes.length).toBe(FLASH_SIZE_BYTES);
    expect([...out.bytes.slice(0x10, 0x13)]).toEqual([1, 2, 3]);
    expect(out.bytes[0]).toBe(0xff);
    expect(out.bytes[FLASH_SIZE_BYTES - 1]).toBe(0xff);
  });

  test("refuses a part that runs past the end of the flash part", () => {
    const parts = new Map([["app.bin", new Uint8Array(0x100)]]);
    const out = assembleMerged("flash_args", [{ offset: FLASH_SIZE_BYTES - 1, value: "app.bin" }], parts);
    expect(out.ok).toBe(false);
  });
});

describe("a dropped build directory", () => {
  function buildDir(separator: string): Drop {
    return {
      root: "build",
      files: [
        text(
          "flasher_args.json",
          JSON.stringify({
            flash_files: {
              "0x0": `bootloader${separator}bootloader.bin`,
              "0x8000": `partition_table${separator}partition-table.bin`,
              "0x10000": "pk-app.bin",
            },
          }),
        ),
        file("bootloader/bootloader.bin", mergedBin(0x4000)),
        file("partition_table/partition-table.bin", new Uint8Array(0xc00).fill(0xaa)),
        file("pk-app.bin", new Uint8Array(0x2000).fill(7)),
        file("pk-app.elf", elf()),
        file("bootloader/bootloader.elf", elf()),
        text("project_description.json", "{}"),
      ],
    };
  }

  test("assembles the merged image from `flash_files` and takes the ELFs beside it", async () => {
    const result = await loadDrop(buildDir("/"));
    expect(result.ok).toBe(true);
    if (!result.ok) {
      return;
    }
    expect(result.image.name).toBe("build");
    expect(result.image.assets.map((asset) => asset.kind)).toEqual([
      LoadKind.MergedFlash,
      LoadKind.AppElf,
      LoadKind.BootloaderElf,
    ]);
    const merged = result.image.assets[0]?.bytes as Uint8Array;
    expect(merged.length).toBe(FLASH_SIZE_BYTES);
    expect(merged[0]).toBe(IMAGE_MAGIC);
    expect(merged[0x8000]).toBe(0xaa);
    expect(merged[0x10000]).toBe(7);
    expect(result.image.notes.join(" ")).toContain("pk-app.bin at 0x10000");
  });

  test("the backslash fixture loads the same image", async () => {
    const slash = await loadDrop(buildDir("/"));
    const backslash = await loadDrop(buildDir("\\"));
    expect(backslash.ok).toBe(true);
    if (!slash.ok || !backslash.ok) {
      return;
    }
    expect(backslash.image.assets.map((a) => a.kind)).toEqual(slash.image.assets.map((a) => a.kind));
    expect(backslash.image.assets[0]?.bytes).toEqual(slash.image.assets[0]?.bytes as Uint8Array);
  });

  test("refuses a `flash_files` value that leaves the directory, in either spelling", async () => {
    for (const escape of ["..\\..\\etc\\passwd", "../../etc/passwd", "/etc/passwd", "C:\\secrets\\app.bin"]) {
      const drop: Drop = {
        root: "build",
        files: [text("flash_args", `0x0 ${escape}\n`), file("app.bin", mergedBin())],
      };
      const reason = await refusal(drop);
      expect(reason, escape).toContain("flash_args");
      // Nothing outside the drop was opened: the refusal quotes the value and stops there.
      expect(reason, escape).toContain(escape);
    }
  });

  test("refuses a `flash_files` value the directory does not hold", async () => {
    const drop: Drop = { root: "build", files: [text("flash_args", "0x0 bootloader/bootloader.bin\n")] };
    expect(await refusal(drop)).toContain("is not in the dropped directory");
  });

  test("refuses a directory with no `flash_args` and nothing loadable in it", async () => {
    const drop: Drop = {
      root: "build",
      files: [text("CMakeLists.txt", "project(pk)"), text("sdkconfig", "CONFIG_X=y")],
    };
    const reason = await refusal(drop);
    expect(reason).toContain("flasher_args.json");
    expect(reason).toContain("flash_args");
  });
});

describe("a dropped file", () => {
  test("a merged bin alone boots", async () => {
    expect(await kinds({ root: null, files: [file("FoloToy-AI-Passport-8MB.bin", mergedBin())] })).toEqual([
      LoadKind.MergedFlash,
    ]);
  });

  test("an ELF alone loads as the app ELF, which is what `inspect` needs", async () => {
    const result = await loadDrop({ root: null, files: [file("FoloToy-AI-Passport.elf", elf())] });
    expect(result.ok && result.image.assets.map((a) => a.kind)).toEqual([LoadKind.AppElf]);
    expect(result.ok && result.image.notes.join(" ")).toContain("symbols only");
  });

  test("a `.pebundle` alone loads as kind 1, which the core takes by its magic", async () => {
    expect(await kinds({ root: null, files: [file("official.pebundle", bundle())] })).toEqual([LoadKind.MergedFlash]);
  });

  test("a bin and its ELF are the pair the corpus has", async () => {
    const drop: Drop = {
      root: null,
      files: [file("FoloToy-AI-Passport-8MB.bin", mergedBin()), file("FoloToy-AI-Passport.elf", elf())],
    };
    expect(await kinds(drop)).toEqual([LoadKind.MergedFlash, LoadKind.AppElf]);
    const result = await loadDrop(drop);
    expect(result.ok && result.image.name).toBe("FoloToy-AI-Passport-8MB");
  });

  test("a corpus directory is its bin, its app ELF and its bootloader ELF", async () => {
    const drop: Drop = {
      root: "pk",
      files: [
        file("FoloToy-AI-Passport-8MB.bin", mergedBin()),
        file("FoloToy-AI-Passport.elf", elf()),
        file("bootloader.elf", elf()),
      ],
    };
    expect(await kinds(drop)).toEqual([LoadKind.MergedFlash, LoadKind.AppElf, LoadKind.BootloaderElf]);
    const result = await loadDrop(drop);
    expect(result.ok && result.image.name).toBe("pk");
  });
});

describe("refusals are values the page states, and name no host path", () => {
  test("a wrong magic", async () => {
    const reason = await refusal({ root: null, files: [file("notes.txt", new Uint8Array([0x68, 0x69, 0x0a]))] });
    expect(reason).toContain("is not a firmware image");
    expect(reason).toContain("notes.txt");
    expect(reason).toContain("68 69 0a");
  });

  test("a too-large file", async () => {
    const huge: DropFile = {
      path: "disk.img",
      size: MAX_FILE_BYTES + 1,
      read: () => Promise.reject(new Error("the loader must refuse this before reading it")),
    };
    expect(await refusal({ root: null, files: [huge] })).toContain(humanSize(MAX_FILE_BYTES));
  });

  test("a merged bin over the size of the flash part", async () => {
    const over: DropFile = {
      path: "big.bin",
      size: FLASH_SIZE_BYTES + 1,
      read: () => Promise.resolve(mergedBin(FLASH_SIZE_BYTES + 1)),
    };
    expect(await refusal({ root: null, files: [over] })).toContain("of the flash part");
  });

  test("an unreadable bundle", async () => {
    const truncated = bundle().slice(0, 14);
    expect(await refusal({ root: null, files: [file("official.pebundle", truncated)] })).toContain("unreadable");
    expect(bundleFault("a.pebundle", new Uint8Array(4))).toContain("too short");
    expect(bundleFault("a.pebundle", bundle())).toBeNull();
  });

  test("an empty drop", async () => {
    expect(await refusal({ root: null, files: [] })).toBe("nothing was dropped");
  });

  test("two merged images in one directory", async () => {
    const drop: Drop = {
      root: "builds",
      files: [file("a.bin", mergedBin()), file("b.bin", mergedBin())],
    };
    expect(await refusal(drop)).toContain("drop the one to run");
  });
});

describe("imageName", () => {
  test("a directory keeps its name and a file loses its extension", () => {
    expect(imageName("/Users/someone/corpus/pk", true)).toBe("pk");
    expect(imageName("C:\\corpus\\pk\\", true)).toBe("pk");
    expect(imageName("FoloToy-AI-Passport-8MB.bin", false)).toBe("FoloToy-AI-Passport-8MB");
    expect(imageName("official.pebundle", false)).toBe("official");
    expect(imageName(".hidden", false)).toBe(".hidden");
    expect(imageName("", false)).toBe("image");
  });
});

describe("whether an image carries its application ELF", () => {
  async function image(drop: Drop) {
    const result = await loadDrop(drop);
    if (!result.ok) {
      throw new Error(`expected a load, got: ${result.reason}`);
    }
    return result.image;
  }

  const manifest = (roles: readonly string[]) =>
    `[bundle]\nid = "pk"\n${roles.map((role) => `\n[[file]]\nrole = "${role}"\nname = "${role}"\nlen = 0\nsha256 = "${"0".repeat(64)}"\n`).join("")}`;

  test("a bare merged bin does not; the bin dropped with its ELF does", async () => {
    expect(carriesAppElf(await image({ root: null, files: [file("keys.bin", mergedBin())] }))).toBe(false);
    expect(carriesAppElf(await image({ root: null, files: [file("keys.bin", mergedBin()), file("keys.elf", elf())] }))).toBe(true);
  });

  test("a .pebundle does exactly when its manifest lists an app_elf payload", async () => {
    expect(bundleCarriesAppElf(bundle(manifest(["flash", "app_elf"])))).toBe(true);
    expect(bundleCarriesAppElf(bundle(manifest(["flash", "boot_elf"])))).toBe(false);
    expect(bundleCarriesAppElf(bundle(manifest(["flash"])))).toBe(false);
    expect(carriesAppElf(await image({ root: null, files: [file("a.pebundle", bundle(manifest(["flash", "app_elf"])))] }))).toBe(true);
    expect(carriesAppElf(await image({ root: null, files: [file("a.pebundle", bundle(manifest(["flash"])))] }))).toBe(false);
  });

  test("a truncated bundle or plain bytes never read as carrying one", () => {
    expect(bundleCarriesAppElf(new Uint8Array(4))).toBe(false);
    expect(bundleCarriesAppElf(new TextEncoder().encode('role = "app_elf"'))).toBe(false);
  });
});
