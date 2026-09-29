// The contract every panel renderer meets, so the session does not care which one the Worker got.

import type { PanelView } from "./rgb565";
import type { Viewport } from "./viewport";

export type DisplayBackend = "webgl1" | "canvas2d";

export interface PanelSink {
  readonly backend: DisplayBackend;
  /**
   * Takes rows `[first, last]` of the whole-panel `pixels`. It may keep the view to redraw after a
   * context loss, and must cope with that view being detached later.
   */
  upload(pixels: Uint16Array, first: number, last: number): void;
  draw(panel: PanelView, canvasWidth: number, canvasHeight: number): Viewport;
}

/** What the page is told about the display, so a fallback is visible rather than silent. */
export interface DisplayState {
  readonly backend: DisplayBackend | "none";
  /** Why the display is not on WebGL1 or may be inexact there; `null` on exact WebGL1. */
  readonly reason: string | null;
  readonly contextLost: boolean;
}
