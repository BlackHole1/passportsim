// Where the browser specs find the wasm core, and when a test may skip for a missing image.
//
// The core is cargo's output, never `bun run build`'s, looked for in order at:
// 1. `PEMU_E2E_CORE`, which `xtask ci` sets to the core it built (`tiers.rs` `CORE_ENV`);
// 2. `target/wasm32-unknown-unknown/wasm-release/pemu_wasm.wasm` (under `CARGO_TARGET_DIR` when set);
// 3. `web/dist/pemu_wasm.wasm`, a packaged bundle's layout.
// A `PEMU_E2E_CORE` naming a missing file throws.
//
// Exactly two absences skip, so a broken core never turns into green skips:
// 1. no wasm core at any of the places above;
// 2. `PEMU_E2E_IMAGE_<NAME>` unset (`pk` and `demo` are never committed). With no corpus here the
//    reason reads like the Rust corpus skips (corpus id `<name>` unavailable), which
//    `outcome::skip_kind` classifies SKIPPED-CORPUS; with a corpus and the variable unset, the
//    reason starts with `blocked: ` so `xtask ci` records it BLOCKED with what it waits on.
// A variable naming a path this host lacks throws.
//
// {@link imageSource} resolves the variable (a merged bin, an ELF, a `.pebundle`, an `idf.py` build
// or corpus directory) to files and a drop root, and {@link imageName} derives the name the page
// will show, with the loader's own rule.

import { existsSync, readdirSync, statSync } from "node:fs";
import { basename, dirname, isAbsolute, join } from "node:path";
import { fileURLToPath } from "node:url";
import { FLASH_ARGS_NAMES, imageName } from "../src/app/load";

const WEB = join(dirname(fileURLToPath(import.meta.url)), "..");
const REPO = join(WEB, "..");

/** The core file the Worker loads beside `worker.js` (`worker.ts` `CORE_FILE`). */
export const CORE_FILE = "pemu_wasm.wasm";

export const CORE_VARIABLE = "PEMU_E2E_CORE";

export function coreCandidates(env: Readonly<Record<string, string | undefined>>): string[] {
  const given = env[CORE_VARIABLE];
  if (given !== undefined && given !== "") {
    return [given];
  }
  const targetDir = env.CARGO_TARGET_DIR
    ? isAbsolute(env.CARGO_TARGET_DIR)
      ? env.CARGO_TARGET_DIR
      : join(REPO, env.CARGO_TARGET_DIR)
    : join(REPO, "target");
  return [join(targetDir, "wasm32-unknown-unknown", "wasm-release", CORE_FILE), join(WEB, "dist", CORE_FILE)];
}

/** The core to serve, or the skip reason naming every place that was looked at. */
export function findCore(
  env: Readonly<Record<string, string | undefined>>,
  exists: (path: string) => boolean = existsSync,
): { path: string } | { skip: string } {
  const candidates = coreCandidates(env);
  const given = env[CORE_VARIABLE];
  if (given !== undefined && given !== "" && !exists(given)) {
    throw new Error(`${CORE_VARIABLE} names ${given}, which does not exist: the run asked for that core`);
  }
  const path = candidates.find(exists);
  if (path !== undefined) {
    return { path };
  }
  return {
    skip: `no wasm core: none at ${candidates.join(", ")}; set ${CORE_VARIABLE}, or run \`cargo build -p pemu-wasm --lib --target wasm32-unknown-unknown --profile wasm-release\``,
  };
}

export interface PreconditionInput {
  readonly core: string | null;
  readonly coreSkip?: string;
  readonly env: Readonly<Record<string, string | undefined>>;
}

/** What a test is and what it waits on besides an image, for the BLOCKED reason. */
export interface RowNeeds {
  readonly name: string;
  readonly waitsOn: string;
}

export interface ImageFile {
  /** The file's path inside the drop, `/`-separated; a bare name for a single dropped file. */
  readonly relative: string;
  /** The host path `setInputFiles` uploads from. It never reaches the page. */
  readonly path: string;
}

export interface ImageSource {
  readonly image: string;
  readonly variable: string;
  readonly root: string;
  /** True when the variable named a directory, which is fed to the `webkitdirectory` input. */
  readonly directory: boolean;
  /** What the page will call the image (`load.ts` `imageName`), and so what `status` answers as `fw`. */
  readonly name: string;
  readonly files: readonly ImageFile[];
}

export type Precondition = { readonly kind: "run"; readonly source: ImageSource } | { readonly kind: "skip"; readonly reason: string };

/** The route `serve.ts` publishes an image's files under, for the drag-and-drop test. */
export const IMAGE_ROUTE = "/e2e-image";

/** Where `serve.ts` serves one file of one image; only names the browser already has appear in it. */
export function imageUrl(image: string, relative: string): string {
  return `${IMAGE_ROUTE}/${image}/${relative}`;
}

/** The most files an image directory may hold, so a mistyped variable cannot walk a home directory. */
const MAX_IMAGE_FILES = 512;

function walk(dir: string, prefix = ""): ImageFile[] {
  const out: ImageFile[] = [];
  for (const name of readdirSync(dir).sort()) {
    const path = join(dir, name);
    const relative = prefix === "" ? name : `${prefix}/${name}`;
    const stat = statSync(path);
    if (stat.isDirectory()) {
      out.push(...walk(path, relative));
    } else if (stat.isFile()) {
      out.push({ relative, path });
    }
    if (out.length > MAX_IMAGE_FILES) {
      throw new Error(`the image directory ${dir} holds more than ${MAX_IMAGE_FILES} files`);
    }
  }
  return out;
}

/** The firmware `PEMU_E2E_IMAGE_<NAME>` names, or `null` when unset; a path this host lacks throws. */
export function imageSource(
  image: string,
  env: Readonly<Record<string, string | undefined>> = process.env,
): ImageSource | null {
  const variable = imageVariable(image);
  const root = env[variable];
  if (root === undefined || root === "") {
    return null;
  }
  if (!existsSync(root)) {
    throw new Error(`${variable} names ${root}, which does not exist: the run asked for that image`);
  }
  const directory = statSync(root).isDirectory();
  return {
    image,
    variable,
    root,
    directory,
    name: imageName(root, directory),
    files: directory ? walk(root) : [{ relative: basename(root), path: root }],
  };
}

export function hasFlashArgs(source: ImageSource): boolean {
  return source.files.some((file) => (FLASH_ARGS_NAMES as readonly string[]).includes(file.relative));
}

export function imageVariable(image: string): string {
  return `PEMU_E2E_IMAGE_${image.toUpperCase().replace(/[^A-Z0-9]/g, "_")}`;
}

/** The data root override, the only root `pemu-testkit` reads. */
export const DATA_ROOT_VARIABLE = "PASSPORTSIM_DATA_ROOT";

/**
 * Why this host has no corpus for `image`, or `null` when it has one. A blank or unset
 * `PASSPORTSIM_DATA_ROOT` is no data root (`pemu_testkit::corpus::data_root_from_env`), and
 * `<data root>/corpus/<image>/` is what `tiers.rs` `e2e_image_env` requires before setting the
 * variable.
 */
export function corpusAbsence(
  image: string,
  env: Readonly<Record<string, string | undefined>>,
  isDirectory: (path: string) => boolean = (path) => existsSync(path) && statSync(path).isDirectory(),
): string | null {
  const root = env[DATA_ROOT_VARIABLE];
  if (root === undefined || root.trim() === "") {
    return `corpus id \`${image}\` unavailable: no data root: set ${DATA_ROOT_VARIABLE} to an absolute path`;
  }
  if (!isDirectory(join(root, "corpus", image))) {
    return `corpus id \`${image}\` unavailable: no \`corpus/${image}\` directory below the data root`;
  }
  return null;
}

export function imagePrecondition(
  image: string,
  row: RowNeeds,
  input: PreconditionInput,
  isDirectory?: (path: string) => boolean,
): Precondition {
  if (input.core === null) {
    return { kind: "skip", reason: input.coreSkip ?? "no wasm core" };
  }
  const variable = imageVariable(image);
  const source = imageSource(image, input.env);
  const absent = source === null ? corpusAbsence(image, input.env, isDirectory) : null;
  if (absent !== null) {
    // Not BLOCKED: nothing is unmet, the host simply has no firmware to run.
    return {
      kind: "skip",
      reason: `${absent}; so no \`${image}\` image can be given (${variable}) and ${row.name} does not run on this host (the corpus is macOS-only)`,
    };
  }
  if (source === null) {
    return {
      kind: "skip",
      reason: `blocked: ${row.name}: no \`${image}\` image given (set ${variable} to a merged bin, an ELF, a \`.pebundle\` or a build directory; FoloToy builds are never committed), and the firmware result waits on ${row.waitsOn}`,
    };
  }
  return { kind: "run", source };
}

export function currentPrecondition(image: string, row: RowNeeds): Precondition {
  const core = findCore(process.env);
  return imagePrecondition(image, row, {
    core: "path" in core ? core.path : null,
    coreSkip: "skip" in core ? core.skip : undefined,
    env: process.env,
  });
}
