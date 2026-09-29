// How large the device is drawn: a percentage of its nominal size, or fit, which never drops below
// {@link FIT_MIN_PERCENT} while the column allows. Sized by arithmetic rather than CSS, because the
// glass must land on whole device pixels before layout, and so a unit test can check that no
// viewport down to 320 px scrolls sideways. The glass comes first and is snapped to a whole number
// of device pixels per guest pixel when that is within {@link SNAP_TOLERANCE}; elsewhere the
// browser resamples the panel smoothly.

import { DEFAULT_ZOOM, type Mode, type Zoom } from "./prefs";
import { PANEL_HEIGHT, PANEL_WIDTH } from "./scale";
import { CSS_PX_PER_MM, DEVICE_MM, SCREEN_MM } from "./skinGeometry";

/** The widest viewport that counts as narrow, inclusive: the environment cards start collapsed. */
export const NARROW_MAX_WIDTH_PX = 400;

export const STACK_BELOW_PX = 900;

export const PAGE_GUTTER_PX = 16;

export const HEADER_HEIGHT_PX = 56;

export const TOOLBAR_HEIGHT_PX = 56;

export const PAGE_PADDING_Y_PX = 48;

export const SIMPLE_CONTROLS_HEIGHT_PX = 108;

export const SIDE_COLUMN_PX = 400;
export const COLUMN_GAP_PX = 64;

export const ADVANCED_DEVICE_SHARE = 0.4;

export const LABEL_GUTTER_PX = 52;

export const SNAP_TOLERANCE = 0.08;

export interface Viewport {
  readonly width: number;
  readonly height: number;
}

export interface DeviceFit {
  /** CSS pixels per device millimetre; every length of the skin is its millimetres times this. */
  readonly pxPerMm: number;
  readonly percent: number;
  readonly capped: boolean;
  readonly glass: {
    readonly cssWidth: number;
    readonly cssHeight: number;
    readonly deviceWidth: number;
    readonly scale: number;
    readonly pixelated: boolean;
  };
  readonly bodyWidth: number;
  readonly bodyHeight: number;
  /** The whole drawing's width: the photo, including the side buttons, and their names. */
  readonly frameWidth: number;
}

export interface DeviceLayout {
  readonly narrow: boolean;
  readonly stacked: boolean;
  readonly device: DeviceFit;
}

export const FIT_MIN_PERCENT = 140;

export function zoomPercent(zoom: Zoom): number | null {
  return zoom === "fit" ? null : Number(zoom);
}

export function fitDevice(zoom: Zoom, columnWidth: number, height: number, devicePixelRatio = 1): DeviceFit {
  const dpr = devicePixelRatio > 0 && Number.isFinite(devicePixelRatio) ? devicePixelRatio : 1;
  const widthK = Math.max(0, columnWidth - 2 * LABEL_GUTTER_PX) / DEVICE_MM.width;
  const heightK = Math.max(0, height) / DEVICE_MM.height;
  const percent = zoomPercent(zoom);
  // Fit is bounded by the height too, a chosen zoom only by the width: the page may scroll down but
  // never sideways.
  const limitK = percent === null ? Math.min(widthK, Math.max(heightK, (CSS_PX_PER_MM * FIT_MIN_PERCENT) / 100)) : widthK;
  const wanted = percent === null ? limitK : (CSS_PX_PER_MM * percent) / 100;
  const k0 = Math.min(wanted, limitK);
  // Snap to a whole scale when one is close and fits, else to a multiple of three so the 3:4
  // height is whole too.
  const exact = SCREEN_MM.width * k0 * dpr;
  const limit = SCREEN_MM.width * limitK * dpr;
  const whole = Math.max(1, Math.round(exact / PANEL_WIDTH));
  const snapped = whole * PANEL_WIDTH;
  const near = Math.abs(exact - snapped) <= SNAP_TOLERANCE * snapped;
  const nearest = 3 * Math.round(exact / 3);
  const deviceWidth = Math.max(3, near && snapped <= limit ? snapped : nearest <= limit ? nearest : 3 * Math.floor(limit / 3));
  const k = deviceWidth / dpr / SCREEN_MM.width;
  const scale = deviceWidth / PANEL_WIDTH;
  return {
    pxPerMm: k,
    percent: Math.round((k / CSS_PX_PER_MM) * 100),
    capped: percent !== null && wanted > widthK,
    glass: {
      cssWidth: deviceWidth / dpr,
      cssHeight: (deviceWidth * PANEL_HEIGHT) / PANEL_WIDTH / dpr,
      deviceWidth,
      scale,
      pixelated: Number.isInteger(scale) || scale >= 2,
    },
    bodyWidth: DEVICE_MM.width * k,
    bodyHeight: DEVICE_MM.height * k,
    frameWidth: DEVICE_MM.width * k + 2 * LABEL_GUTTER_PX,
  };
}

export function deviceLayout(mode: Mode, viewport: Viewport, devicePixelRatio = 1, zoom: Zoom = DEFAULT_ZOOM): DeviceLayout {
  const narrow = viewport.width <= NARROW_MAX_WIDTH_PX;
  const stacked = viewport.width < STACK_BELOW_PX;
  const content = Math.max(0, viewport.width - 2 * PAGE_GUTTER_PX);
  let columnWidth: number;
  let height: number;
  if (mode === "simple") {
    columnWidth = stacked ? content : content - SIDE_COLUMN_PX - COLUMN_GAP_PX;
    height = viewport.height - HEADER_HEIGHT_PX - PAGE_PADDING_Y_PX - SIMPLE_CONTROLS_HEIGHT_PX;
  } else {
    columnWidth = stacked ? content : Math.floor(viewport.width * ADVANCED_DEVICE_SHARE);
    height = viewport.height - HEADER_HEIGHT_PX - TOOLBAR_HEIGHT_PX - PAGE_PADDING_Y_PX;
  }
  return { narrow, stacked, device: fitDevice(zoom, columnWidth, height, devicePixelRatio) };
}

/** The widest the laid-out content is: the device, plus simple mode's side column when not stacked. */
export function contentWidth(mode: Mode, layout: DeviceLayout): number {
  const beside = mode === "simple" && !layout.stacked ? COLUMN_GAP_PX + SIDE_COLUMN_PX : 0;
  return layout.device.frameWidth + beside + 2 * PAGE_GUTTER_PX;
}

export function overflowsHorizontally(mode: Mode, layout: DeviceLayout, viewport: Viewport): boolean {
  return contentWidth(mode, layout) > viewport.width;
}
