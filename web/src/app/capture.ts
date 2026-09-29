// Pictures of the screen (the Screenshot PNG and the history thumbnail), drawn from the Worker's
// panel memory through the last reported panel state, so they show what the user sees.

import { toRgba, type PanelView } from "../gl/rgb565";
import { formatVirtualTime } from "./header";

export interface RawFrame {
  readonly width: number;
  readonly height: number;
  readonly pixels: Uint16Array;
}

export const LIT_PANEL: PanelView = {
  backlight: 1,
  backlightScale: 1,
  glassComplement: false,
  powered: true,
  sleeping: false,
  displayOn: true,
};

export function frameRgba(frame: RawFrame, panel: PanelView): Uint8ClampedArray | null {
  if (frame.width <= 0 || frame.height <= 0 || frame.pixels.length < frame.width * frame.height) {
    return null;
  }
  const out = new Uint8ClampedArray(frame.width * frame.height * 4);
  toRgba(frame.pixels.subarray(0, frame.width * frame.height), panel, out);
  return out;
}

/**
 * The screenshot's file name: the image and the virtual instant, so shots sort by time and never
 * overwrite. Only `[A-Za-z0-9._-]` survive from the image name.
 */
export function screenshotName(image: string, nowPs: bigint): string {
  const safe = image.replace(/[^A-Za-z0-9._-]+/g, "_").replace(/^[._]+/, "") || "passportsim";
  return `${safe}-vt${formatVirtualTime(nowPs).replace(" ", "")}.png`;
}

export const THUMBNAIL_SCALE = 0.5;

/** Encodes RGBA as PNG at `scale` (nearest-neighbour when enlarging, smoothed when shrinking). */
export type PngEncoder = (rgba: Uint8ClampedArray, width: number, height: number, scale: number) => Promise<Blob | null>;

export const canvasPng: PngEncoder = async (rgba, width, height, scale) => {
  const source = document.createElement("canvas");
  source.width = width;
  source.height = height;
  const context = source.getContext("2d");
  if (!context) {
    return null;
  }
  const data = context.createImageData(width, height);
  data.data.set(rgba);
  context.putImageData(data, 0, 0);
  let canvas = source;
  if (scale !== 1) {
    canvas = document.createElement("canvas");
    canvas.width = Math.max(1, Math.round(width * scale));
    canvas.height = Math.max(1, Math.round(height * scale));
    const scaled = canvas.getContext("2d");
    if (!scaled) {
      return null;
    }
    scaled.imageSmoothingEnabled = scale < 1;
    scaled.drawImage(source, 0, 0, canvas.width, canvas.height);
  }
  return new Promise((resolve) => {
    canvas.toBlob((blob) => {
      resolve(blob);
    }, "image/png");
  });
};

export function downloadBlob(blob: Blob, name: string): void {
  const url = URL.createObjectURL(blob);
  const link = document.createElement("a");
  link.href = url;
  link.download = name;
  link.rel = "noopener";
  document.body.appendChild(link);
  link.click();
  link.remove();
  // Revoked later, not at once: Firefox reads the URL after `click` returns.
  setTimeout(() => {
    URL.revokeObjectURL(url);
  }, 30_000);
}
