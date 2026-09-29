import { AlertCircleIcon, RotateCcwIcon, UploadIcon } from "lucide-react";
import { useLayoutEffect, useRef, type MouseEvent, type PointerEvent, type ReactNode } from "react";
import { Button } from "../../ui/button";
import { Spinner } from "../../ui/spinner";
import { downloadAmount, downloading, downloadPercent, downloadText, downloadTitle } from "../download";
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
      {empty ? null : <GlassBoot />}
      {props.children}
    </>
  );
}

/**
 * The glass while the machine it will show is not up yet: what the page is waiting for, with the
 * bytes a download has brought so far, or why it failed and a retry. Without it the glass is black
 * and reads as a device that is off or paused.
 */
function GlassBoot() {
  const page = usePage();
  const boot = useStore(page.boot);
  const t = useT();
  if (!boot.starting) {
    return null;
  }
  const { download } = boot;
  const text = "text-[clamp(0.5rem,6cqw,0.9375rem)] font-medium leading-snug";
  const small = "text-[clamp(0.4375rem,4.8cqw,0.75rem)] leading-snug tabular-nums";
  const failed = download?.error !== undefined || boot.failure !== null;
  if (failed) {
    const detail = download?.error !== undefined ? downloadText(t, download) : (boot.failure ?? "");
    return (
      <div className="glass-boot @container absolute inset-0 flex flex-col items-center justify-center gap-[5%] bg-black p-[8%] text-center text-white/80" data-glass-boot="failed" role="alert">
        <AlertCircleIcon aria-hidden="true" className="size-[14%] max-h-9 max-w-9 min-h-3 min-w-3 text-white/72" />
        <p className={text}>{t("device.failed")}</p>
        <p className={`${small} line-clamp-4 break-words text-white/60`}>{detail}</p>
        <Button
          className="mt-[2%]"
          data-action="retry"
          onClick={() => {
            page.actions.retry();
          }}
          size="xs"
          variant="outline"
        >
          <RotateCcwIcon aria-hidden="true" />
          {t("download.retry")}
        </Button>
      </div>
    );
  }
  const active = downloading(download) ? download : null;
  const percent = active === null ? null : downloadPercent(active.received, active.total);
  const amount = active === null ? "" : downloadAmount(t, active);
  return (
    <div
      aria-busy="true"
      className="glass-boot @container absolute inset-0 flex flex-col items-center justify-center gap-[5%] bg-black p-[10%] text-center text-white/80"
      data-glass-boot={active === null ? "starting" : active.what}
    >
      <Spinner aria-hidden="true" aria-label={undefined} className="size-[12%] max-h-8 max-w-8 min-h-3 min-w-3 text-white/64" role={undefined} />
      <p className={text}>{active === null ? t("device.starting") : downloadTitle(t, active)}</p>
      {percent === null ? null : (
        <div
          aria-label={downloadTitle(t, active!)}
          aria-valuemax={100}
          aria-valuemin={0}
          aria-valuenow={percent}
          className="h-1 w-4/5 overflow-hidden rounded-full bg-white/16"
          role="progressbar"
        >
          <div className="h-full rounded-full bg-white/80 transition-[width] duration-200 ease-out" style={{ width: `${percent}%` }} />
        </div>
      )}
      <p className={`${small} min-h-[1lh] text-white/60`} data-glass-amount="">
        {amount}
      </p>
    </div>
  );
}
