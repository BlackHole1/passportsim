import { UploadIcon } from "lucide-react";
import { useLayoutEffect, useRef, type MouseEvent, type PointerEvent, type ReactNode } from "react";
import { runsNothing } from "../loader";
import { highlightBox } from "../panels/uiTree";
import { usePage, useStore, useT } from "./hooks";

export type DeviceButton = "up" | "ok" | "down" | "power";

/**
 * Pointer handlers that make a device button physical: down presses, and release, cancel or
 * leaving releases. No pointer capture, so sliding off releases as on a real button. Only the main
 * button presses, so two fingers can hold two buttons; the long-touch context menu is suppressed
 * because it would take the pointer mid-hold.
 */
export function usePress() {
  const page = usePage();
  const edge = (id: DeviceButton, down: boolean) => {
    if (id === "power") {
      page.actions.power(down);
    } else {
      page.actions.button(id, down);
    }
  };
  return (id: DeviceButton) => ({
    onPointerDown: (event: PointerEvent) => {
      if (event.button === 0) {
        edge(id, true);
      }
    },
    onPointerUp: () => {
      edge(id, false);
    },
    onPointerCancel: () => {
      edge(id, false);
    },
    onPointerLeave: () => {
      edge(id, false);
    },
    onLostPointerCapture: () => {
      edge(id, false);
    },
    onContextMenu: (event: MouseEvent) => {
      event.preventDefault();
    },
  });
}

/**
 * The glass and the rewound-frame overlay. The canvases are the page's, appended rather than
 * rendered, because a canvas React re-created would be one the Worker never draws to.
 */
export function GlassHost(props: { readonly width: number; readonly height: number; readonly pixelated: boolean; readonly children?: ReactNode }) {
  const page = usePage();
  const highlight = useStore(page.highlight);
  const loader = useStore(page.loader.store);
  const empty = runsNothing(loader) && loader.state !== "loading";
  const t = useT();
  const host = useRef<HTMLDivElement>(null);
  const { width, height, pixelated } = props;

  useLayoutEffect(() => {
    host.current?.append(page.glass, page.rewindCanvas);
  }, [page]);

  useLayoutEffect(() => {
    for (const canvas of [page.glass, page.rewindCanvas]) {
      canvas.style.width = `${width}px`;
      canvas.style.height = `${height}px`;
      canvas.style.imageRendering = pixelated ? "" : "auto";
    }
  }, [page, width, height, pixelated]);

  const box = highlight === null ? null : highlightBox(highlight.rect, highlight.screen, { width, height });
  return (
    <>
      <div className="skin-glass absolute inset-0 [&>canvas]:absolute [&>canvas]:inset-0" ref={host} />
      {box === null ? null : (
        <div
          aria-hidden="true"
          className="ui-highlight pointer-events-none absolute rounded-[2px] outline-2 outline-white outline-offset-1 shadow-[0_0_0_3px_rgb(0_0_0/0.6)]"
          style={{ left: box.left, top: box.top, width: box.width, height: box.height }}
        />
      )}
      {empty ? (
        <div className="glass-empty @container absolute inset-0 flex flex-col items-center justify-center gap-[6%] p-[8%] text-center text-white/72" data-glass-empty="">
          <UploadIcon aria-hidden="true" className="size-[16%] max-h-10 max-w-10 min-h-3 min-w-3" />
          <p className="text-[clamp(0.5rem,6cqw,0.9375rem)] font-medium leading-snug">{t("device.empty")}</p>
        </div>
      ) : null}
      {props.children}
    </>
  );
}
