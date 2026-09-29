// The device photo as a data URL, imported as a Bun macro so the bundle carries the bytes: a
// package ships only the files `xtask package` lists, and an emitted image would not be one.

import { readFileSync } from "node:fs";

export function devicePhoto(): string {
  const bytes = readFileSync(new URL("./device-front.webp", import.meta.url));
  return `data:image/webp;base64,${bytes.toString("base64")}`;
}
