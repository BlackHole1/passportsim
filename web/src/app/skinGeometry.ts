// The device photo's geometry in millimetres. The photo (`view/device-front.webp`, cropped from
// FoloToy's front product photo) is 558 x 883 px for the spec sheet's 60 x 95 mm, so 9.29 px/mm.
// 100 % zoom is the CSS millimetre, a nominal size rather than a measured one.

export interface MmBox {
  readonly x: number;
  readonly y: number;
  readonly width: number;
  readonly height: number;
}

export interface EdgeControl {
  readonly id: "up" | "ok" | "down" | "power";
  readonly edge: "left" | "right";
  readonly box: MmBox;
}

export const PHOTO_PX = { width: 558, height: 883 } as const;

export const DEVICE_MM = { width: 60, height: 95 } as const;

const MM_PER_PX = DEVICE_MM.height / PHOTO_PX.height;

function mm(x: number, y: number, width: number, height: number): MmBox {
  return { x: x * MM_PER_PX, y: y * MM_PER_PX, width: width * MM_PER_PX, height: height * MM_PER_PX };
}

/**
 * The glass: the photo's screen is 284 x 378 px from (103, 118); the 3:4 canvas is 284 px wide and
 * centred vertically. The photo's own screen content is painted out a little past this edge.
 */
export const SCREEN_MM: MmBox = mm(103, 118 + (378 - (284 * 4) / 3) / 2, 284, (284 * 4) / 3);

export const SCREEN_RADIUS_MM = 31 * MM_PER_PX;

/** UP, OK and DOWN on the right edge at a 138 px pitch, POWER on the left level with UP. */
export const EDGE_CONTROLS: readonly EdgeControl[] = [
  { id: "up", edge: "right", box: mm(540, 127, 18, 92) },
  { id: "ok", edge: "right", box: mm(540, 265, 18, 92) },
  { id: "down", edge: "right", box: mm(540, 403, 18, 92) },
  { id: "power", edge: "left", box: mm(0, 128, 18, 90) },
];

/** CSS Values 4: 1in = 96px = 25.4mm. */
export const CSS_PX_PER_MM = 96 / 25.4;

export function boxPx(box: MmBox, k: number): { left: number; top: number; width: number; height: number } {
  return { left: box.x * k, top: box.y * k, width: box.width * k, height: box.height * k };
}
