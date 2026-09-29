// The device as it looks: the front photo, with the glass and side buttons as elements over it at
// the same scale, every length from `skinGeometry.ts`.

import { useLayoutEffect, useRef } from "react";
import { cn } from "../../ui/lib/utils";
import type { DeviceFit } from "../layout";
import { DEVICE_MM, EDGE_CONTROLS, SCREEN_MM, SCREEN_RADIUS_MM, boxPx, type EdgeControl } from "../skinGeometry";
import { devicePhoto } from "./devicePhoto" with { type: "macro" };
import { GlassHost, usePress } from "./glass";
import { usePage, useStore, useT } from "./hooks";

const DEVICE_PHOTO = devicePhoto();

/** A side button. The name beside it is part of the target, since the part itself can be a few pixels wide. */
function EdgeButton(props: {
  readonly control: EdgeControl;
  readonly k: number;
  readonly gutter: number;
  readonly label: string;
  readonly aria: string;
  readonly disabled: boolean;
}) {
  const press = usePress();
  const { control, k, gutter } = props;
  const box = boxPx(control.box, k);
  const right = control.edge === "right";
  const width = box.width + gutter;
  return (
    <button
      aria-label={props.aria}
      className={cn(
        "edge-button group absolute flex items-center gap-1.5 outline-none select-none touch-none disabled:cursor-not-allowed",
        right ? "flex-row" : "flex-row-reverse",
      )}
      data-control={control.id}
      disabled={props.disabled}
      style={{
        top: box.top,
        height: box.height,
        width,
        ...(right ? { left: box.left } : { right: DEVICE_MM.width * k - box.left - box.width }),
      }}
      title={props.label}
      type="button"
      {...press(control.id)}
    >
      <span
        aria-hidden="true"
        className={cn(
          "edge-press h-full shrink-0 rounded-[3px] transition-colors group-focus-visible:ring-2 group-focus-visible:ring-ring",
          !props.disabled && "group-hover:bg-white/25 group-active:bg-black/40",
        )}
        style={{ width: box.width }}
      />
      <span
        aria-hidden="true"
        className="truncate font-medium text-[0.625rem] text-muted-foreground group-hover:text-foreground group-disabled:opacity-50"
      >
        {props.label}
      </span>
    </button>
  );
}

export function DeviceBody(props: { readonly fit: DeviceFit; readonly gutter: number }) {
  const page = usePage();
  const t = useT();
  const disabled = useStore(page.skin).inputDisabled;
  const { fit } = props;
  const k = fit.pxPerMm;
  const screen = boxPx(SCREEN_MM, k);

  // The glass must start on a whole device pixel, or the browser resamples every guest pixel across
  // two screen pixels, so it is nudged onto the grid whenever the page changes size. The photo's
  // screen is painted black past its edge, so the nudge never uncovers it.
  const glass = useRef<HTMLDivElement>(null);
  useLayoutEffect(() => {
    const node = glass.current;
    if (!node) {
      return;
    }
    const snap = () => {
      node.style.translate = "";
      const rect = node.getBoundingClientRect();
      const dpr = window.devicePixelRatio || 1;
      const off = (value: number) => (Math.round(value * dpr) - value * dpr) / dpr;
      const dx = off(rect.left + window.scrollX);
      const dy = off(rect.top + window.scrollY);
      node.style.translate = dx === 0 && dy === 0 ? "" : `${dx}px ${dy}px`;
    };
    snap();
    // A line under the device that grows or wraps moves the glass without resizing the body.
    const observer = typeof ResizeObserver === "function" ? new ResizeObserver(snap) : null;
    for (const watched of [document.body, node.closest(".device-column"), node.closest(".layout")]) {
      if (watched !== null) {
        observer?.observe(watched);
      }
    }
    window.addEventListener("resize", snap);
    return () => {
      observer?.disconnect();
      window.removeEventListener("resize", snap);
    };
  }, [fit.glass.cssWidth, fit.glass.cssHeight]);

  useLayoutEffect(() => {
    page.glass.setAttribute("aria-label", t("device.screen"));
    page.rewindCanvas.setAttribute("aria-label", t("device.rewound"));
  }, [page, t]);

  return (
    <div className="device-drawing relative" style={{ paddingInline: props.gutter }}>
      <div className="device-body relative" data-device-body="" style={{ width: fit.bodyWidth, height: fit.bodyHeight }}>
        <img
          alt=""
          aria-hidden="true"
          className="device-photo pointer-events-none absolute inset-0 size-full max-w-none select-none drop-shadow-[0_18px_28px_rgb(0_0_0/0.28)]"
          draggable={false}
          src={DEVICE_PHOTO}
        />
        <div
          className="skin-screen absolute overflow-hidden bg-black"
          ref={glass}
          style={{ left: screen.left, top: screen.top, width: fit.glass.cssWidth, height: fit.glass.cssHeight, borderRadius: SCREEN_RADIUS_MM * k }}
        >
          <GlassHost height={fit.glass.cssHeight} pixelated={fit.glass.pixelated} width={fit.glass.cssWidth} />
        </div>
        <div aria-label={t("device.buttons")} role="group">
          {EDGE_CONTROLS.map((control) => (
            <EdgeButton
              aria={t(`device.${control.id}.aria`)}
              control={control}
              disabled={disabled}
              gutter={props.gutter}
              k={k}
              key={control.id}
              label={t(`device.${control.id}`)}
            />
          ))}
        </div>
      </div>
    </div>
  );
}
