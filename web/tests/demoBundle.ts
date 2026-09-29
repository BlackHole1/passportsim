// The `official.pebundle` the browser specs serve beside `worker.js`, built from the corpus exactly
// as `xtask/src/package/demo.rs` builds the published one. FoloToy builds are never committed: the
// files come from `PEMU_E2E_OFFICIAL_DIR`, or `builds/official/build` under `PASSPORTSIM_DATA_ROOT`.

import { createHash } from "node:crypto";
import { existsSync, readFileSync } from "node:fs";
import { join } from "node:path";

/** The published bundle's files as `demo.rs` `FILES` names them: corpus file, role, name, SHA-256 prefix. */
export const DEMO_FILES = [
  { file: "FoloToy-AI-Passport-8MB.bin", role: "flash", name: "FoloToy-AI-Passport-8MB.bin", shaPrefix: "580285887df4e163" },
  { file: "FoloToy-AI-Passport.elf", role: "app_elf", name: "FoloToy-AI-Passport.elf", shaPrefix: "dd63252a5675a2de" },
] as const;

export const DEMO_ID = "official";
export const DEMO_NAME = "FoloToy AI Passport BSP demo";

export interface DemoFile {
  readonly role: string;
  readonly name: string;
  readonly bytes: Buffer;
  readonly sha256: string;
}

export function officialDir(env: Readonly<Record<string, string | undefined>> = process.env): string | undefined {
  const root = env.PASSPORTSIM_DATA_ROOT;
  return env.PEMU_E2E_OFFICIAL_DIR ?? (root ? join(root, "builds", "official", "build") : undefined);
}

/**
 * The `official` demo files: `skip` names an absent file, `mismatch` a file that is not the pinned
 * build (the caller fails on it).
 */
export function readOfficialFiles(
  env: Readonly<Record<string, string | undefined>> = process.env,
): { files: DemoFile[] } | { skip: string } | { mismatch: string } {
  const dir = officialDir(env);
  const files: DemoFile[] = [];
  for (const demo of DEMO_FILES) {
    const path = dir ? join(dir, demo.file) : undefined;
    if (!path || !existsSync(path)) {
      return {
        skip: `no \`official\` ${demo.role}: set PEMU_E2E_OFFICIAL_DIR or PASSPORTSIM_DATA_ROOT (FoloToy builds are never committed)`,
      };
    }
    const bytes = readFileSync(path);
    const sha256 = createHash("sha256").update(bytes).digest("hex");
    if (!sha256.startsWith(demo.shaPrefix)) {
      return { mismatch: `the ${demo.role} at ${path} is not the pinned \`official\` build (SHA-256 ${sha256})` };
    }
    files.push({ role: demo.role, name: demo.name, bytes, sha256 });
  }
  return { files };
}

/**
 * The published demo bundle, byte for byte what `pemu_loader::bundle::build` writes (the manifest,
 * then the payloads in order); `tests/milestones/m9.rs` rebuilds it and checks the SHA-256. `bundle`
 * names another id and name, for a test that drops a different firmware in the same form
 * (`wifiHttp.spec.ts`).
 */
export function pebundle(
  files: readonly DemoFile[],
  bundle: { readonly id?: string; readonly name?: string } = {},
): Buffer {
  let manifest = `[bundle]\nid = "${bundle.id ?? DEMO_ID}"\nname = "${bundle.name ?? DEMO_NAME}"\n`;
  for (const file of files) {
    manifest += `\n[[file]]\nrole = "${file.role}"\nname = "${file.name}"\nlen = ${file.bytes.length}\nsha256 = "${file.sha256}"\n`;
  }
  const header = Buffer.alloc(12);
  header.write("PEBUNDL1", 0, "ascii");
  header.writeUInt32LE(Buffer.byteLength(manifest), 8);
  return Buffer.concat([header, Buffer.from(manifest, "utf8"), ...files.map((file) => file.bytes)]);
}
