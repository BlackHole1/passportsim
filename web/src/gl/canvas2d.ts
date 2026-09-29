// The 2D-context fallback when a Worker gets no WebGL1 context. Rows go through a 65,536-entry LUT
// into one panel-sized `ImageData`, are put into a scratch canvas with dirty-row `putImageData`,
// and are blitted at the integer scale with smoothing off. The LUT bakes in the complement and the
// backlight, so it is rebuilt (and every row re-converted) only when those change.

import { DirtySpan } from "./dirty";
import { fillRgbaLut, panelGain, type PanelView } from "./rgb565";
import type { PanelSink } from "./sink";
import { ViewportCache, type Viewport } from "./viewport";

export interface PixelImage {
  readonly width: number;
  readonly height: number;
  readonly data: Uint8ClampedArray;
}

export interface ScratchContext {
  createImageData(width: number, height: number): PixelImage;
  putImageData(
    image: PixelImage,
    dx: number,
    dy: number,
    dirtyX: number,
    dirtyY: number,
    dirtyWidth: number,
    dirtyHeight: number,
  ): void;
}

export interface TargetContext<S> {
  imageSmoothingEnabled: boolean;
  fillStyle: unknown;
  fillRect(x: number, y: number, width: number, height: number): void;
  drawImage(
    source: S,
    sx: number,
    sy: number,
    sw: number,
    sh: number,
    dx: number,
    dy: number,
    dw: number,
    dh: number,
  ): void;
}

export class Canvas2dRenderer<S> implements PanelSink {
  readonly backend = "canvas2d" as const;
  private readonly image: PixelImage;
  private readonly words: Uint32Array;
  private readonly lut = new Uint32Array(65536);
  /** The LUT's two inputs, compared field by field: `gain * 2 + complement` collides at 1.5. */
  private lutGain = Number.NaN;
  private lutComplement = false;
  private readonly span: DirtySpan;
  private readonly viewports: ViewportCache;
  private pixels: Uint16Array | null = null;

  /** `target` is the visible canvas; `scratch` a second canvas of exactly `width x height`. */
  constructor(
    private readonly target: TargetContext<S>,
    private readonly scratch: ScratchContext,
    private readonly scratchCanvas: S,
    private readonly width: number,
    private readonly height: number,
  ) {
    this.image = scratch.createImageData(width, height);
    this.words = new Uint32Array(this.image.data.buffer, this.image.data.byteOffset, width * height);
    this.span = new DirtySpan(height);
    this.viewports = new ViewportCache(width, height);
  }

  upload(pixels: Uint16Array, first: number, last: number): void {
    this.pixels = pixels;
    this.span.add(first, last);
  }

  draw(panel: PanelView, canvasWidth: number, canvasHeight: number): Viewport {
    const gain = panelGain(panel);
    if (gain !== this.lutGain || panel.glassComplement !== this.lutComplement) {
      fillRgbaLut(this.lut, panel);
      this.lutGain = gain;
      this.lutComplement = panel.glassComplement;
      this.span.addAll();
    }
    const pixels = this.pixels;
    if (this.span.pending && pixels && pixels.length >= this.width * this.height) {
      const first = this.span.firstRow;
      const last = this.span.lastRow;
      const words = this.words;
      const lut = this.lut;
      const end = (last + 1) * this.width;
      for (let index = first * this.width; index < end; index += 1) {
        words[index] = lut[pixels[index] as number] as number;
      }
      this.scratch.putImageData(this.image, 0, 0, 0, first, this.width, last - first + 1);
      this.span.clear();
    }
    const viewport = this.viewports.get(canvasWidth, canvasHeight);
    const target = this.target;
    target.fillStyle = "#000";
    target.fillRect(0, 0, canvasWidth, canvasHeight);
    target.imageSmoothingEnabled = false;
    target.drawImage(
      this.scratchCanvas,
      0,
      0,
      this.width,
      this.height,
      viewport.x,
      viewport.y,
      viewport.width,
      viewport.height,
    );
    return viewport;
  }
}
