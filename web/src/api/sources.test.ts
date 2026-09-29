// The api sources stay text: a raw control byte makes git treat a file as binary,
// so its diffs stop being reviewable.

import { expect, test } from "bun:test";
import { readdirSync, readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));

test("no source under web/src/api holds a raw control byte other than tab, LF and CR", () => {
  for (const name of readdirSync(HERE).filter((file) => file.endsWith(".ts"))) {
    const bytes = readFileSync(join(HERE, name));
    const bad = [...bytes].findIndex((b) => (b < 0x20 && b !== 0x09 && b !== 0x0a && b !== 0x0d) || b === 0x7f);
    expect({ name, bad }).toEqual({ name, bad: -1 });
  }
});
