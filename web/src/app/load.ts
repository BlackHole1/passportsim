// The image loader, without a DOM. Every rule about a drop is decided here on names the browser
// gave, so no refusal can print a host path, and nothing outside the drop is ever opened: a
// `flash_files` value is resolved by lookup in the dropped set. The ROM is compiled into the core,
// so no branch produces a `RomElf` asset, and eFuse stays the built-in image.

import { LoadKind } from "../worker/layout";
import { inflateIfGzip } from "../worker/gzip";

/** The firmware the page boots with no input and names in its header. */
export const DEMO_IMAGE = "official";

/** The board's flash size: a merged image is at most this. */
export const FLASH_SIZE_BYTES = 8 * 1024 * 1024;

/** What an erased cell of that part reads, and so what a merged image is padded with. */
export const ERASED_BYTE = 0xff;

/** The largest file the page reads, so a dropped disk image is refused by size instead of read. */
export const MAX_FILE_BYTES = 64 * 1024 * 1024;

/** The largest drop, so a dropped home directory is refused rather than walked. */
export const MAX_DROP_BYTES = 256 * 1024 * 1024;

export const MAX_DROP_FILES = 4096;

/** `PEBUNDL1`, the `.pebundle` magic (`crates/pemu-loader/src/bundle.rs` `BUNDLE_MAGIC`). */
export const BUNDLE_MAGIC = "PEBUNDL1";

/** `ESP_IMAGE_HEADER_MAGIC` (`crates/pemu-loader/src/esp_image.rs` `IMAGE_MAGIC`). */
export const IMAGE_MAGIC = 0xe9;

export const TABLE_OFFSET = 0x8000;

/** The names `idf.py build` writes its offset-and-file list under, in lookup order. */
export const FLASH_ARGS_NAMES = ["flasher_args.json", "flash_args"] as const;

export interface DropFile {
  /** Path relative to the dropped directory, `/`-separated, or the bare name of a loose file. */
  readonly path: string;
  readonly size: number;
  read(): Promise<Uint8Array>;
}

export interface Drop {
  readonly root: string | null;
  readonly files: readonly DropFile[];
}

export interface ImageAsset {
  readonly kind: LoadKind;
  readonly bytes: Uint8Array;
  /** The file name the bytes came from; a history download uses it. */
  readonly file?: string;
}

export interface LoadedImage {
  readonly name: string;
  readonly assets: readonly ImageAsset[];
  readonly notes: readonly string[];
}

/** One step of a load, emitted after the work is done, never ahead of it. */
export type LoadStep =
  | { readonly kind: "received"; readonly files: number; readonly bytes: number }
  | { readonly kind: "detected"; readonly what: "bundle" | "merged-bin" | "elf" | "build-directory" | "files" }
  | { readonly kind: "read"; readonly path: string; readonly bytes: number }
  | { readonly kind: "assembled"; readonly list: string; readonly parts: number; readonly bytes: number };

export type LoadProgress = (step: LoadStep) => void;

export type LoadResult = { readonly ok: true; readonly image: LoadedImage } | { readonly ok: false; readonly reason: string };

function refuse(reason: string): LoadResult {
  return { ok: false, reason };
}

export function humanSize(bytes: number): string {
  const units = ["B", "KiB", "MiB", "GiB"];
  let value = bytes;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  return `${unit === 0 ? value : value.toFixed(1)} ${units[unit]}`;
}

/**
 * The name the page shows for a drop and sends as `fw`: a directory keeps its name and a file
 * loses its last extension. Deliberately no smarter, so the name is predictable from the drop
 * (`tests/preconditions.ts` derives the same one).
 */
export function imageName(dropName: string, isDirectory: boolean): string {
  const base = dropName.replace(/[\\/]+$/, "").split(/[\\/]/).pop()?.trim() ?? "";
  if (base === "") {
    return "image";
  }
  if (isDirectory) {
    return base;
  }
  const dot = base.lastIndexOf(".");
  return dot > 0 ? base.slice(0, dot) : base;
}

/**
 * One `flash_files` value as a path inside the drop. Splits on both separators, because a
 * `flasher_args.json` written on Windows carries `\`; rejects an absolute path and any `..`.
 */
export function relativePath(value: string): { readonly ok: true; readonly path: string } | { readonly ok: false; readonly reason: string } {
  const raw = value.trim();
  if (raw === "") {
    return { ok: false, reason: "it is empty" };
  }
  if (/^[\\/]/.test(raw)) {
    return { ok: false, reason: "it is an absolute path" };
  }
  if (/^[A-Za-z]:/.test(raw)) {
    return { ok: false, reason: "it names a drive" };
  }
  const segments: string[] = [];
  for (const segment of raw.split(/[\\/]+/)) {
    if (segment === "" || segment === ".") {
      continue;
    }
    if (segment === "..") {
      return { ok: false, reason: "it leaves the dropped directory with `..`" };
    }
    segments.push(segment);
  }
  if (segments.length === 0) {
    return { ok: false, reason: "it names no file" };
  }
  return { ok: true, path: segments.join("/") };
}

export interface FlashEntry {
  readonly offset: number;
  readonly value: string;
}

/**
 * The `flash_files` of a build directory: `flasher_args.json` (JSON, offset to file) or
 * `flash_args` (`--opt value` pairs, then `0x<offset> <file>` pairs, as
 * `xtask/src/probes/build.rs` `assemble_merged` reads it). The whole text is tokenized at once so
 * both layouts parse.
 */
export function parseFlashFiles(name: string, text: string): { readonly ok: true; readonly entries: FlashEntry[] } | { readonly ok: false; readonly reason: string } {
  const entries: FlashEntry[] = [];
  if (name.endsWith(".json")) {
    let parsed: unknown;
    try {
      parsed = JSON.parse(text);
    } catch {
      return { ok: false, reason: `\`${name}\` is not JSON` };
    }
    const files = (parsed as { flash_files?: unknown } | null)?.flash_files;
    if (typeof files !== "object" || files === null) {
      return { ok: false, reason: `\`${name}\` has no \`flash_files\` object` };
    }
    for (const [offset, value] of Object.entries(files as Record<string, unknown>)) {
      if (typeof value !== "string") {
        return { ok: false, reason: `\`${name}\`: the entry at ${offset} is not a file name` };
      }
      const at = Number(offset);
      if (!Number.isInteger(at) || at < 0) {
        return { ok: false, reason: `\`${name}\`: \`${offset}\` is not an offset` };
      }
      entries.push({ offset: at, value });
    }
  } else {
    const tokens = text.split(/\s+/).filter((token) => token !== "");
    for (let i = 0; i < tokens.length; i += 1) {
      const token = tokens[i] ?? "";
      if (token.startsWith("--")) {
        i += 1;
        continue;
      }
      if (!/^0[xX][0-9a-fA-F]+$/.test(token)) {
        continue;
      }
      const value = tokens[i + 1];
      if (value === undefined) {
        return { ok: false, reason: `\`${name}\`: the offset ${token} names no file` };
      }
      i += 1;
      entries.push({ offset: Number.parseInt(token.slice(2), 16), value });
    }
  }
  if (entries.length === 0) {
    return { ok: false, reason: `\`${name}\` lists no \`flash_files\`` };
  }
  entries.sort((a, b) => a.offset - b.offset);
  return { ok: true, entries };
}

export function isBundle(bytes: Uint8Array): boolean {
  return BUNDLE_MAGIC.split("").every((char, i) => bytes[i] === char.charCodeAt(0));
}

export function isElf(bytes: Uint8Array): boolean {
  return bytes[0] === 0x7f && bytes[1] === 0x45 && bytes[2] === 0x4c && bytes[3] === 0x46;
}

export function isMergedBin(bytes: Uint8Array): boolean {
  return bytes[0] === IMAGE_MAGIC;
}

/** Why a `.pebundle` is truncated or not one at all, or `null`. The core parses the manifest. */
export function bundleFault(name: string, bytes: Uint8Array): string | null {
  if (bytes.length < 12) {
    return `\`${name}\` is ${humanSize(bytes.length)}: too short to be a \`.pebundle\``;
  }
  const manifestLen = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength).getUint32(8, true);
  if (manifestLen === 0 || manifestLen > bytes.length - 12) {
    return `\`${name}\` is an unreadable \`.pebundle\`: its manifest says ${manifestLen} bytes and the file holds ${bytes.length - 12}`;
  }
  return null;
}

/** Whether a `.pebundle` lists an `app_elf` payload, from the manifest's `role` lines only. */
export function bundleCarriesAppElf(bytes: Uint8Array): boolean {
  if (!isBundle(bytes) || bundleFault("bundle", bytes) !== null) {
    return false;
  }
  const manifestLen = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength).getUint32(8, true);
  const manifest = new TextDecoder().decode(bytes.subarray(12, 12 + manifestLen));
  return /^\s*role\s*=\s*"app_elf"\s*$/m.test(manifest);
}

/**
 * Whether an image carries its application ELF. Without it the UI tree and settle detection have
 * no symbols, and a radio binds only on an exact module match, so the page says what is missing.
 */
export function carriesAppElf(image: LoadedImage): boolean {
  return image.assets.some(
    (asset) => asset.kind === LoadKind.AppElf || (asset.kind === LoadKind.MergedFlash && bundleCarriesAppElf(asset.bytes)),
  );
}

function baseName(path: string): string {
  return path.split("/").pop() ?? path;
}

export function assembleMerged(
  listName: string,
  entries: readonly FlashEntry[],
  parts: ReadonlyMap<string, Uint8Array>,
): { readonly ok: true; readonly bytes: Uint8Array; readonly placed: string[] } | { readonly ok: false; readonly reason: string } {
  const image = new Uint8Array(FLASH_SIZE_BYTES).fill(ERASED_BYTE);
  const placed: string[] = [];
  for (const entry of entries) {
    const resolved = relativePath(entry.value);
    if (!resolved.ok) {
      return { ok: false, reason: `\`${listName}\` names \`${entry.value}\`, which is refused: ${resolved.reason}` };
    }
    const bytes = parts.get(resolved.path);
    if (bytes === undefined) {
      return { ok: false, reason: `\`${listName}\` names \`${resolved.path}\`, which is not in the dropped directory` };
    }
    const end = entry.offset + bytes.length;
    if (end > image.length) {
      return {
        ok: false,
        reason: `\`${resolved.path}\` at 0x${entry.offset.toString(16)} runs past the end of an ${humanSize(FLASH_SIZE_BYTES)} image`,
      };
    }
    image.set(bytes, entry.offset);
    placed.push(`${resolved.path} at 0x${entry.offset.toString(16)}`);
  }
  return { ok: true, bytes: image, placed };
}

function sizeFault(drop: Drop): string | null {
  if (drop.files.length === 0) {
    return "nothing was dropped";
  }
  if (drop.files.length > MAX_DROP_FILES) {
    return `the drop holds ${drop.files.length} files, over the ${MAX_DROP_FILES} the page reads`;
  }
  let total = 0;
  for (const file of drop.files) {
    if (file.size > MAX_FILE_BYTES) {
      return `\`${baseName(file.path)}\` is ${humanSize(file.size)}, over the ${humanSize(MAX_FILE_BYTES)} the page reads`;
    }
    total += file.size;
  }
  if (total > MAX_DROP_BYTES) {
    return `the drop is ${humanSize(total)}, over the ${humanSize(MAX_DROP_BYTES)} the page reads`;
  }
  return null;
}

async function readFile(file: DropFile, onStep: LoadProgress): Promise<Uint8Array> {
  const bytes = await inflateIfGzip(await file.read());
  onStep({ kind: "read", path: file.path, bytes: bytes.length });
  return bytes;
}

/** A single dropped file: a `.pebundle`, a merged bin or an ELF, by its magic. */
async function loadOneFile(file: DropFile, onStep: LoadProgress): Promise<LoadResult> {
  const name = baseName(file.path);
  const bytes = await readFile(file, onStep);
  const what = isBundle(bytes) ? "bundle" : isElf(bytes) ? "elf" : isMergedBin(bytes) ? "merged-bin" : null;
  if (what !== null) {
    onStep({ kind: "detected", what });
  }
  if (isBundle(bytes)) {
    const fault = bundleFault(name, bytes);
    return fault !== null
      ? refuse(fault)
      : {
          ok: true,
          image: {
            name: imageName(name, false),
            assets: [{ kind: LoadKind.MergedFlash, bytes, file: name }],
            notes: [`bundle ${name} (${humanSize(bytes.length)})`],
          },
        };
  }
  if (isElf(bytes)) {
    return {
      ok: true,
      image: {
        name: imageName(name, false),
        // An ELF alone boots nothing (the core boots from flash); it gives `inspect` its symbols.
        assets: [{ kind: LoadKind.AppElf, bytes, file: name }],
        notes: [`app ELF ${name} (${humanSize(bytes.length)}), symbols only: no flash image was dropped with it`],
      },
    };
  }
  if (isMergedBin(bytes)) {
    if (bytes.length <= TABLE_OFFSET) {
      return refuse(`\`${name}\` is ${humanSize(bytes.length)}: a merged flash image holds the partition table at 0x8000`);
    }
    if (bytes.length > FLASH_SIZE_BYTES) {
      return refuse(`\`${name}\` is ${humanSize(bytes.length)}, over the ${humanSize(FLASH_SIZE_BYTES)} of the flash part`);
    }
    return {
      ok: true,
      image: {
        name: imageName(name, false),
        assets: [{ kind: LoadKind.MergedFlash, bytes, file: name }],
        notes: [`flash ${name} (${humanSize(bytes.length)})`],
      },
    };
  }
  const head = [...bytes.slice(0, 4)].map((byte) => byte.toString(16).padStart(2, "0")).join(" ");
  return refuse(
    `\`${name}\` is not a firmware image: it starts with ${head === "" ? "nothing" : head} and the page loads a merged bin (0xe9), an ELF, or a \`.pebundle\``,
  );
}

function flashArgsEntry(files: ReadonlyMap<string, DropFile>): DropFile | null {
  for (const name of FLASH_ARGS_NAMES) {
    const found = files.get(name);
    if (found !== undefined) {
      return found;
    }
  }
  return null;
}

/**
 * A dropped directory, or several files at once: an `idf.py` build directory when it carries a
 * flash-args file, otherwise loose files (a merged bin beside its ELFs). An unrecognized file is
 * ignored, so one stray file does not cost the drop; a drop that yields nothing is refused.
 */
async function loadDirectory(drop: Drop, rootName: string, onStep: LoadProgress): Promise<LoadResult> {
  const byPath = new Map<string, DropFile>();
  for (const file of drop.files) {
    const resolved = relativePath(file.path);
    if (resolved.ok) {
      byPath.set(resolved.path, file);
    }
  }
  const assets: ImageAsset[] = [];
  const notes: string[] = [];
  let primary: string | null = null;

  // The app ELF is the one ELF that is not `bootloader/bootloader.elf`.
  const elfPaths: string[] = [];
  const bootElfPaths: string[] = [];
  const binPaths: string[] = [];
  const bundlePaths: string[] = [];
  for (const path of byPath.keys()) {
    const base = baseName(path).toLowerCase();
    if (base.endsWith(".pebundle")) {
      bundlePaths.push(path);
    } else if (base.endsWith(".elf")) {
      (base === "bootloader.elf" ? bootElfPaths : elfPaths).push(path);
    } else if (base.endsWith(".bin")) {
      binPaths.push(path);
    }
  }

  const list = flashArgsEntry(byPath);
  onStep({ kind: "detected", what: bundlePaths.length > 0 ? "bundle" : list !== null ? "build-directory" : "files" });
  if (bundlePaths.length > 1) {
    return refuse(`\`${rootName}\` holds ${bundlePaths.length} \`.pebundle\` files (${bundlePaths.join(", ")}); drop one of them`);
  }
  const bundlePath = bundlePaths[0];
  if (bundlePath !== undefined) {
    const file = byPath.get(bundlePath);
    const bytes = await readFile(file as DropFile, onStep);
    const fault = bundleFault(baseName(bundlePath), bytes);
    if (fault !== null) {
      return refuse(fault);
    }
    assets.push({ kind: LoadKind.MergedFlash, bytes, file: baseName(bundlePath) });
    notes.push(`bundle ${bundlePath} (${humanSize(bytes.length)})`);
    primary = bundlePath;
  } else if (list !== null) {
    const listName = baseName(list.path);
    const parsed = parseFlashFiles(listName, new TextDecoder().decode(await readFile(list, onStep)));
    if (!parsed.ok) {
      return refuse(parsed.reason);
    }
    const parts = new Map<string, Uint8Array>();
    for (const entry of parsed.entries) {
      const resolved = relativePath(entry.value);
      if (!resolved.ok) {
        return refuse(`\`${listName}\` names \`${entry.value}\`, which is refused: ${resolved.reason}`);
      }
      const part = byPath.get(resolved.path);
      if (part === undefined) {
        return refuse(`\`${listName}\` names \`${resolved.path}\`, which is not in the dropped directory`);
      }
      parts.set(resolved.path, await readFile(part, onStep));
    }
    const merged = assembleMerged(listName, parsed.entries, parts);
    if (!merged.ok) {
      return refuse(merged.reason);
    }
    onStep({ kind: "assembled", list: listName, parts: parsed.entries.length, bytes: merged.bytes.length });
    assets.push({ kind: LoadKind.MergedFlash, bytes: merged.bytes });
    notes.push(`flash from ${listName}: ${merged.placed.join(", ")}`);
    primary = list.path;
  } else {
    // No list: a merged bin is the one `.bin` that starts with the image magic. The bytes are kept
    // from the check, because a merged image is megabytes.
    const merged: { path: string; bytes: Uint8Array }[] = [];
    for (const path of binPaths) {
      const file = byPath.get(path) as DropFile;
      if (file.size <= TABLE_OFFSET || file.size > FLASH_SIZE_BYTES) {
        continue;
      }
      const bytes = await readFile(file, onStep);
      if (isMergedBin(bytes)) {
        merged.push({ path, bytes });
      }
    }
    if (merged.length > 1) {
      return refuse(
        `\`${rootName}\` holds ${merged.length} merged images (${merged.map((one) => one.path).join(", ")}); drop the one to run`,
      );
    }
    const found = merged[0];
    if (found !== undefined) {
      assets.push({ kind: LoadKind.MergedFlash, bytes: found.bytes, file: baseName(found.path) });
      notes.push(`flash ${found.path} (${humanSize(found.bytes.length)})`);
      primary = found.path;
    }
  }

  if (elfPaths.length > 1) {
    return refuse(`\`${rootName}\` holds ${elfPaths.length} application ELFs (${elfPaths.join(", ")}); drop the one to run`);
  }
  const appElf = elfPaths[0];
  if (appElf !== undefined) {
    const bytes = await readFile(byPath.get(appElf) as DropFile, onStep);
    if (!isElf(bytes)) {
      return refuse(`\`${appElf}\` is named like an ELF and does not start with the ELF magic`);
    }
    assets.push({ kind: LoadKind.AppElf, bytes, file: baseName(appElf) });
    notes.push(`app ELF ${appElf} (${humanSize(bytes.length)})`);
    primary ??= appElf;
  }
  const bootElf = bootElfPaths[0];
  if (bootElf !== undefined && bootElfPaths.length === 1) {
    const bytes = await readFile(byPath.get(bootElf) as DropFile, onStep);
    if (isElf(bytes)) {
      assets.push({ kind: LoadKind.BootloaderElf, bytes, file: baseName(bootElf) });
      notes.push(`bootloader ELF ${bootElf} (${humanSize(bytes.length)})`);
    }
  }

  if (assets.length === 0) {
    return refuse(
      `\`${rootName}\` has no \`flasher_args.json\` or \`flash_args\` with \`flash_files\`, and no merged bin, ELF or \`.pebundle\` in it`,
    );
  }
  // Loose files are named after the flash image among them, so a bin with its ELF is named as the
  // bin alone would be.
  const name = drop.root !== null ? imageName(drop.root, true) : imageName(baseName(primary ?? rootName), false);
  return {
    ok: true,
    image: { name, assets: assets.map((asset) => (asset.file === undefined ? { ...asset, file: `${name}.bin` } : asset)), notes },
  };
}

/**
 * The image a drop carries, or the refusal the page shows. A bad drop resolves to a refusal; only
 * an unreadable file rejects.
 */
export async function loadDrop(drop: Drop, onStep: LoadProgress = () => {}): Promise<LoadResult> {
  const fault = sizeFault(drop);
  if (fault !== null) {
    return refuse(fault);
  }
  onStep({ kind: "received", files: drop.files.length, bytes: drop.files.reduce((sum, file) => sum + file.size, 0) });
  const single = drop.files[0];
  if (drop.root === null && drop.files.length === 1 && single !== undefined) {
    return loadOneFile(single, onStep);
  }
  return loadDirectory(drop, drop.root ?? "the dropped files", onStep);
}
