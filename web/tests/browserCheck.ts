// `bun tests/browserCheck.ts <row>`: prints `PRESENT` or `MISSING <install hint>` for one row of
// `browsers.ts` and exits 0 either way, so `xtask ci` can record the row NOT_RUN with the hint.

import { BROWSER_ROWS, missingBrowser, rowById } from "./browsers";

const row = rowById(process.argv[2] ?? "");
if (row === undefined) {
  console.error(`usage: bun tests/browserCheck.ts <${BROWSER_ROWS.map((r) => r.id).join("|")}>`);
  process.exit(2);
}
const gap = missingBrowser(row);
console.log(gap === null ? "PRESENT" : `MISSING ${gap}`);
