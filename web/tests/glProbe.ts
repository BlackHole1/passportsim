// The browser entry of `gl.spec.ts`: the shipped renderer modules on `globalThis.pemuGl`, bundled
// by the spec and never part of `dist/`.

import { openDisplay } from "../src/gl/display";
import { toRgba } from "../src/gl/rgb565";

(globalThis as { pemuGl?: unknown }).pemuGl = { openDisplay, toRgba };
