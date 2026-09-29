// Where the panel sits in its canvas. Both renderers use this one function, so the WebGL viewport
// and the 2D blit agree. The canvas holds the panel at 1:1; the page's zoom is CSS.

import { integerScale } from "./rgb565";

export interface Viewport {
  readonly scale: number;
  readonly x: number;
  readonly y: number;
  readonly width: number;
  readonly height: number;
  /** Bottom edge in canvas pixels, from the bottom, which is what `gl.viewport` takes. */
  readonly glY: number;
}

/**
 * The panel at the largest integer scale that fits, centred, an odd remainder putting the extra
 * pixel right and below. A canvas smaller than the panel crops at scale 1 rather than scaling down.
 */
export function panelViewport(
  panelWidth: number,
  panelHeight: number,
  canvasWidth: number,
  canvasHeight: number,
): Viewport {
  const scale = integerScale(panelWidth, panelHeight, canvasWidth, canvasHeight);
  const width = panelWidth * scale;
  const height = panelHeight * scale;
  const x = Math.floor((canvasWidth - width) / 2);
  const y = Math.floor((canvasHeight - height) / 2);
  return { scale, x, y, width, height, glY: canvasHeight - y - height };
}

/** {@link panelViewport} for the last canvas size, the same object while the size holds. */
export class ViewportCache {
  private canvasWidth = -1;
  private canvasHeight = -1;
  private cached: Viewport | null = null;

  constructor(
    private readonly panelWidth: number,
    private readonly panelHeight: number,
  ) {}

  get(canvasWidth: number, canvasHeight: number): Viewport {
    if (
      this.cached === null ||
      canvasWidth !== this.canvasWidth ||
      canvasHeight !== this.canvasHeight
    ) {
      this.cached = panelViewport(this.panelWidth, this.panelHeight, canvasWidth, canvasHeight);
      this.canvasWidth = canvasWidth;
      this.canvasHeight = canvasHeight;
    }
    return this.cached;
  }
}
