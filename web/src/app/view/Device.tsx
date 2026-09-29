// The device view: the photo with its glass and side buttons, the zoom, and the status lines.
// Advanced mode adds the USB strip, the NFC zone and the skin's own lines.

import { type ReactNode } from "react";
import { Kbd } from "../../ui/kbd";
import { cn } from "../../ui/lib/utils";
import { Spinner } from "../../ui/spinner";
import { downloading, downloadText } from "../download";
import { LABEL_GUTTER_PX } from "../layout";
import { DEMO_IMAGE } from "../load";
import { runsNothing } from "../loader";
import { ZOOMS, type Zoom } from "../prefs";
import { displayNotice, RAIL_ONLY_STATES, USB_STATES, type SkinStatus } from "../skin";
import type { Translate } from "../i18n";
import type { DisplayState } from "../../gl/sink";
import { DeviceBody } from "./DeviceBody";
import { usePage, useStore, useT } from "./hooks";
import { StopNotice } from "./notices";

export function Device() {
  const page = usePage();
  const { mode, zoom } = useStore(page.prefs);
  const t = useT();
  const fit = useStore(page.layout).device;
  return (
    <section
      aria-label={t("device.label")}
      className="device-column flex min-w-0 flex-col items-center gap-5"
      data-percent={fit.percent}
      data-scale={fit.glass.scale}
      data-zoom={zoom}
    >
      <DeviceBody fit={fit} gutter={LABEL_GUTTER_PX} />
      <StopNotice />
      <div className="flex flex-wrap items-center justify-center gap-x-5 gap-y-2">
        <KeyHints t={t} />
        <ZoomControl capped={fit.capped} percent={fit.percent} zoom={zoom} />
      </div>
      {mode === "advanced" ? (
        <>
          <UsbStrip />
          <NfcZone />
          <SkinLines />
        </>
      ) : (
        <SimpleStatus />
      )}
    </section>
  );
}

function ZoomControl(props: { readonly zoom: Zoom; readonly percent: number; readonly capped: boolean }) {
  const page = usePage();
  const t = useT();
  const shown = props.zoom === "fit" || props.capped;
  return (
    <div className="zoom flex items-center gap-2">
      <div aria-label={t("zoom.label")} className="flex items-center gap-0.5 rounded-lg bg-muted p-0.5" role="group">
        {ZOOMS.map((zoom) => (
          <button
            aria-pressed={props.zoom === zoom ? "true" : "false"}
            className={cn(
              "h-7 min-w-10 rounded-md px-2 font-medium text-muted-foreground text-xs tabular-nums outline-none transition-colors hover:text-foreground focus-visible:ring-2 focus-visible:ring-ring",
              props.zoom === zoom && "bg-background text-foreground shadow-xs/5 dark:bg-input",
            )}
            data-zoom-choice={zoom}
            key={zoom}
            onClick={() => {
              page.actions.setZoom(zoom);
            }}
            type="button"
          >
            {zoom === "fit" ? t("zoom.fit") : t("zoom.percent", { percent: zoom })}
          </button>
        ))}
      </div>
      {shown ? (
        <span className="text-muted-foreground text-xs tabular-nums" data-zoom-drawn={props.percent}>
          {t(props.capped ? "zoom.capped" : "zoom.drawn", { percent: props.percent })}
        </span>
      ) : null}
    </div>
  );
}

function KeyHints(props: { readonly t: Translate }) {
  const { t } = props;
  const hint = (keys: ReactNode, label: string) => (
    <span className="inline-flex items-center gap-1.5">
      {keys}
      <span>{label}</span>
    </span>
  );
  return (
    <p className="key-hints flex flex-wrap items-center justify-center gap-x-4 gap-y-1 text-muted-foreground text-xs">
      {hint(
        <>
          <Kbd>↑</Kbd>
          <Kbd>↓</Kbd>
        </>,
        t("keys.move"),
      )}
      {hint(<Kbd>Enter</Kbd>, t("keys.ok"))}
      {hint(<Kbd>P</Kbd>, t("keys.power"))}
    </p>
  );
}

function SimpleStatus() {
  const page = usePage();
  const t = useT();
  const header = useStore(page.header);
  const stopped = useStore(page.stop) !== null;
  const empty = runsNothing(useStore(page.loader.store));
  const boot = useStore(page.boot);
  const demo = header.image === DEMO_IMAGE;
  // Until its machine first runs, the page has nothing that could be paused.
  const state = empty
    ? "empty"
    : boot.starting
      ? boot.failure !== null || boot.download?.error !== undefined
        ? "failed"
        : "starting"
      : stopped
        ? "stopped"
        : header.running
          ? "running"
          : "paused";
  const word = state === "starting" && downloading(boot.download) ? downloadText(t, boot.download) : t(`status.${state}`);
  return (
    <div
      aria-busy={state === "starting" ? "true" : undefined}
      className="sim-status flex max-w-full flex-wrap items-center justify-center gap-x-4 gap-y-1 text-sm"
      role="status"
    >
      <span className="inline-flex items-center gap-2" data-running={header.running ? "true" : "false"} data-state={state}>
        {state === "starting" ? (
          <Spinner aria-hidden="true" aria-label={undefined} className="size-3.5 text-muted-foreground" role={undefined} />
        ) : (
          <span
            aria-hidden="true"
            className={cn(
              "size-2 rounded-full",
              state === "running" && "bg-foreground",
              (state === "paused" || state === "empty") && "border border-muted-foreground",
              (state === "stopped" || state === "failed") && "bg-destructive",
            )}
          />
        )}
        <span className={cn("font-medium tabular-nums", (stopped || state === "failed") && "text-destructive-foreground")} data-status-text="">
          {word}
        </span>
      </span>
      {empty || boot.starting ? null : (
        <>
          <span className="max-w-64 truncate text-muted-foreground" data-image={header.image} title={header.image}>
            {demo ? t("status.demo") : header.image}
          </span>
          <span className="font-mono text-muted-foreground text-xs tabular-nums">{t("status.vt", { vt: header.virtualTime })}</span>
          <span className="font-mono text-muted-foreground text-xs tabular-nums" data-speed>
            {t("status.speed", { factor: header.realTimeFactor })}
          </span>
        </>
      )}
    </div>
  );
}


/** The four USB host states. U1 is the power button's, so it is shown and explained, not sent. */
function UsbStrip() {
  const page = usePage();
  const t = useT();
  const skin = useStore(page.skin);
  const railOnly = new Set<string>(RAIL_ONLY_STATES);
  return (
    <div className="flex w-full max-w-96 flex-col items-center gap-1">
      <div aria-label={t("usb.connector")} className="usb-connector grid w-full grid-cols-4 gap-0.5 rounded-lg bg-muted p-0.5" role="group">
        {USB_STATES.map((state) => {
          const active = skin.status.usb === state.id;
          const blocked = railOnly.has(state.id);
          const label = t(`usb.label.${state.id}`);
          return (
            <button
              aria-disabled={blocked || skin.inputDisabled ? "true" : undefined}
              aria-label={t("usb.state.aria", { id: state.id, label })}
              aria-pressed={active ? "true" : "false"}
              className={cn(
                "usb-state flex min-w-0 flex-col items-center rounded-md px-1 py-1 text-muted-foreground leading-tight transition-colors hover:text-foreground",
                active && "active bg-background text-foreground shadow-xs/5 dark:bg-input",
                (blocked || skin.inputDisabled) && "cursor-not-allowed opacity-56 hover:text-muted-foreground",
              )}
              data-usb={state.id}
              key={state.id}
              // `aria-disabled` rather than `disabled`: a disabled button shows no tooltip in some engines, and
              // U1's tooltip explains why it cannot be chosen.
              onClick={() => {
                if (!blocked && !skin.inputDisabled) {
                  page.actions.usbState(state.id);
                }
              }}
              title={blocked ? t("usb.u1Why") : `${label}: ${t(`usb.summary.${state.id}`)}`}
              type="button"
            >
              <span className="w-full truncate text-center font-medium text-[0.6875rem]">{t(`usb.short.${state.id}`)}</span>
              <span className="font-mono text-[0.625rem] opacity-72">{state.id}</span>
            </button>
          );
        })}
      </div>
      <p className="usb-legend text-center text-muted-foreground text-xs">{t("usb.strip.legend")}</p>
    </div>
  );
}

function NfcZone() {
  const page = usePage();
  const t = useT();
  const taps = useStore(page.taps);
  return (
    <div
      aria-label={t("nfc.zone.aria")}
      className="nfc-zone relative flex h-10 w-full max-w-72 items-center justify-center overflow-hidden rounded-lg border border-dashed border-input text-muted-foreground text-xs"
      onDragOver={(event) => {
        event.preventDefault();
      }}
      onDrop={(event) => {
        event.preventDefault();
        const text = event.dataTransfer?.getData("text/plain") ?? "";
        if (text.length > 0) {
          page.actions.nfcDrop(text);
        }
      }}
      role="button"
      tabIndex={0}
    >
      {taps > 0 ? <span aria-hidden="true" className="nfc-ripple absolute inset-0 rounded-lg bg-foreground/15" key={taps} /> : null}
      <span className="relative">{t("nfc.zone")}</span>
    </div>
  );
}

function panelWord(t: Translate, panel: SkinStatus["panel"]): string {
  switch (panel) {
    case null:
      return "?";
    case "on":
      return t("panel.on");
    case "off":
      return t("panel.off");
    case "sleeping":
      return t("panel.sleeping");
    case "display off":
      return t("panel.displayOff");
    case "inverted":
      return t("panel.inverted");
  }
}

export function rendererLine(t: Translate, state: DisplayState): { level: string; text: string } {
  const notice = displayNotice(state);
  const reason = state.reason ?? "";
  switch (notice.level) {
    case "lost":
      return { level: notice.level, text: t("renderer.lost", { backend: state.backend }) };
    case "ok":
      return { level: notice.level, text: t("renderer.webgl") };
    case "inexact":
      return { level: notice.level, text: t("renderer.webglInexact", { reason }) };
    case "fallback":
      return { level: notice.level, text: reason === "" ? t("renderer.fallbackBare") : t("renderer.fallback", { reason }) };
    case "none":
      return { level: notice.level, text: reason === "" ? t("renderer.noneBare") : t("renderer.none", { reason }) };
  }
}

function SkinLines() {
  const page = usePage();
  const t = useT();
  const skin = useStore(page.skin);
  const backlight = skin.status.backlightPercent === null ? "?" : `${skin.status.backlightPercent}%`;
  const renderer = skin.display === null ? null : rendererLine(t, skin.display);
  return (
    <div className="flex flex-col items-center gap-1 text-center font-mono text-muted-foreground text-xs">
      <div className="skin-status" role="status">
        {t("skin.status", { backlight, panel: panelWord(t, skin.status.panel), usb: skin.status.usb })}
      </div>
      <div
        aria-live="polite"
        className={cn("skin-display min-h-4", renderer !== null && renderer.level !== "ok" && "font-medium text-foreground")}
        data-level={renderer?.level}
        role="status"
      >
        {renderer?.text ?? ""}
      </div>
    </div>
  );
}
