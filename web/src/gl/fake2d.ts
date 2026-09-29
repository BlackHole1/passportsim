// A software 2D context for tests of the fallback renderer, with the canvas spec's semantics for
// opaque pixels. A real 2D context runs the same renderer in `web/tests/gl.spec.ts`.

import type { PixelImage, ScratchContext, TargetContext } from "./canvas2d";

export class SoftCanvas {
  readonly rgba: Uint8ClampedArray;

  constructor(
    readonly width: number,
    readonly height: number,
  ) {
    this.rgba = new Uint8ClampedArray(width * height * 4);
  }

  pixel(x: number, y: number): [number, number, number, number] {
    const at = (y * this.width + x) * 4;
    return [this.rgba[at] ?? -1, this.rgba[at + 1] ?? -1, this.rgba[at + 2] ?? -1, this.rgba[at + 3] ?? -1];
  }
}

export class Soft2d implements ScratchContext, TargetContext<SoftCanvas> {
  imageSmoothingEnabled = true;
  fillStyle: unknown = "#000";
  readonly puts: { first: number; rows: number }[] = [];

  constructor(readonly canvas: SoftCanvas) {}

  createImageData(width: number, height: number): PixelImage {
    return { width, height, data: new Uint8ClampedArray(width * height * 4) };
  }

  putImageData(
    image: PixelImage,
    dx: number,
    dy: number,
    dirtyX: number,
    dirtyY: number,
    dirtyWidth: number,
    dirtyHeight: number,
  ): void {
    this.puts.push({ first: dirtyY, rows: dirtyHeight });
    for (let y = dirtyY; y < dirtyY + dirtyHeight; y += 1) {
      for (let x = dirtyX; x < dirtyX + dirtyWidth; x += 1) {
        const from = (y * image.width + x) * 4;
        const to = ((y + dy) * this.canvas.width + x + dx) * 4;
        this.canvas.rgba.set(image.data.subarray(from, from + 4), to);
      }
    }
  }

  fillRect(x: number, y: number, width: number, height: number): void {
    if (this.fillStyle !== "#000") {
      throw new Error(`Soft2d only fills black, not ${String(this.fillStyle)}`);
    }
    for (let row = Math.max(0, y); row < Math.min(this.canvas.height, y + height); row += 1) {
      for (let col = Math.max(0, x); col < Math.min(this.canvas.width, x + width); col += 1) {
        this.canvas.rgba.set([0, 0, 0, 255], (row * this.canvas.width + col) * 4);
      }
    }
  }

  drawImage(
    source: SoftCanvas,
    sx: number,
    sy: number,
    sw: number,
    sh: number,
    dx: number,
    dy: number,
    dw: number,
    dh: number,
  ): void {
    if (this.imageSmoothingEnabled) {
      throw new Error("Soft2d draws only with smoothing off, as the renderer must");
    }
    for (let y = Math.max(0, dy); y < Math.min(this.canvas.height, dy + dh); y += 1) {
      for (let x = Math.max(0, dx); x < Math.min(this.canvas.width, dx + dw); x += 1) {
        const srcX = sx + Math.floor(((x - dx + 0.5) * sw) / dw);
        const srcY = sy + Math.floor(((y - dy + 0.5) * sh) / dh);
        const from = (srcY * source.width + srcX) * 4;
        this.canvas.rgba.set(source.rgba.subarray(from, from + 4), (y * this.canvas.width + x) * 4);
      }
    }
  }
}
