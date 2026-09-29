// Advanced mode's workbench. Mounted in both modes and hidden in simple mode so card state
// survives; shown, it is `display: contents`, so its parts sit in the page grid.

import { ChevronDownIcon, ImageDownIcon, PauseIcon, PlayIcon, SaveIcon, StepForwardIcon } from "lucide-react";
import { useLayoutEffect, useRef, useState, type ReactNode } from "react";
import type { PageModel } from "../page";
import { Button } from "../../ui/button";
import { Card } from "../../ui/card";
import { cn } from "../../ui/lib/utils";
import { Tabs, TabsList, TabsPanel, TabsTab } from "../../ui/tabs";
import { CARDS, TABS, type CardSpec } from "../tabs";
import { AudioCard } from "./cards/Audio";
import { BatteryCard } from "./cards/Battery";
import { BleCard } from "./cards/Ble";
import { NfcCard } from "./cards/Nfc";
import { SnapshotsCard } from "./cards/Snapshots";
import { UsbCard } from "./cards/Usb";
import { WifiCard } from "./cards/Wifi";
import { usePage, useStore, useT } from "./hooks";
import { ConsolePane } from "./panes/Console";
import { EventsPane } from "./panes/Events";
import { FidelityPane } from "./panes/Fidelity";
import { InspectPane } from "./panes/Inspect";
import { PerfPane } from "./panes/Perf";
import { UiTreePane } from "./panes/UiTree";

export function Workbench(props: { readonly hidden: boolean }) {
  return (
    <div className="workbench" hidden={props.hidden}>
      <Toolbar />
      <Panels />
      <Cards />
    </div>
  );
}

/** Opens a card if collapsed, scrolls it into view and marks it briefly, for a confirmation that names it. */
export function revealCard(page: PageModel, id: string): void {
  if (page.tabs.isCollapsed(id, page.layout.get().narrow)) {
    page.actions.toggleCard(id);
  }
  // After the render that opened it, so the scroll lands on the card's full height.
  requestAnimationFrame(() => {
    const card = document.querySelector<HTMLElement>(`[data-card="${id}"]`);
    if (!card) {
      return;
    }
    card.scrollIntoView({ block: "nearest", behavior: "smooth" });
    card.dataset.flash = "true";
    card.querySelector<HTMLElement>(".card-toggle")?.focus({ preventScroll: true });
    setTimeout(() => {
      delete card.dataset.flash;
    }, 1600);
  });
}

function ToolbarNotice(props: { readonly last: "state" | "shot" | null }) {
  const page = usePage();
  const t = useT();
  const saved = useStore(page.snapshots.store);
  const shot = useStore(page.capture);
  if (props.last === "state") {
    if (saved.error?.kind === "refusal") {
      return (
        <p className="toolbar-notice text-destructive-foreground text-xs" data-notice="state-failed" role="status">
          {t("toolbar.stateFailed", { error: saved.error.text })}
        </p>
      );
    }
    if (saved.saved === null) {
      return null;
    }
    return (
      <p className="toolbar-notice flex flex-wrap items-center gap-x-2 text-muted-foreground text-xs" data-notice="state" role="status">
        <span>{t("toolbar.stateSaved", { name: saved.saved.name, at: saved.saved.at })}</span>
        <Button
          data-action="show-snapshots"
          onClick={() => {
            revealCard(page, "snapshots");
          }}
          size="xs"
          variant="outline"
        >
          {t("toolbar.showSnapshots")}
        </Button>
      </p>
    );
  }
  if (props.last === "shot" && shot !== null) {
    return (
      <p
        className={cn("toolbar-notice text-xs", shot.kind === "failed" ? "text-destructive-foreground" : "text-muted-foreground")}
        data-notice={shot.kind === "failed" ? "shot-failed" : "shot"}
        role="status"
      >
        {shot.kind === "failed" ? t("toolbar.shotFailed", { reason: shot.reason }) : t("toolbar.shotSaved", { file: shot.file })}
      </p>
    );
  }
  return null;
}

function Toolbar() {
  const page = usePage();
  const t = useT();
  const header = useStore(page.header);
  const [last, setLast] = useState<"state" | "shot" | null>(null);
  const transport = (action: string, label: string, icon: ReactNode, onClick: () => void, disabled: boolean, title?: string) => (
    <Button className="transport" data-action={action} disabled={disabled} onClick={onClick} size="sm" title={title} variant="ghost">
      {icon}
      {label}
    </Button>
  );
  const field = (name: string, value: string) => (
    <span className="header-field whitespace-nowrap" data-field={name}>
      {value}
    </span>
  );
  return (
    <div className="toolbar flex flex-wrap items-center gap-x-4 gap-y-2 rounded-xl border bg-card px-2 py-1.5 shadow-xs/5">
      <div aria-label={t("toolbar.label")} className="transport-group flex flex-wrap items-center gap-0.5" role="toolbar">
        {transport("run", t("toolbar.run"), <PlayIcon aria-hidden="true" />, page.actions.run, header.running || header.inputDisabled)}
        {transport("pause", t("toolbar.pause"), <PauseIcon aria-hidden="true" />, page.actions.pause, !header.running || header.inputDisabled)}
        {transport("step", t("toolbar.step"), <StepForwardIcon aria-hidden="true" />, page.actions.step, header.running || header.inputDisabled)}
        <span aria-hidden="true" className="mx-1 h-5 w-px bg-border" />
        {transport(
          "snap",
          t("toolbar.snap"),
          <SaveIcon aria-hidden="true" />,
          () => {
            setLast("state");
            page.actions.snapshot();
          },
          header.inputDisabled,
          t("toolbar.snap.title"),
        )}
        {transport(
          "screenshot",
          t("toolbar.screenshot"),
          <ImageDownIcon aria-hidden="true" />,
          () => {
            setLast("shot");
            void page.actions.screenshot();
          },
          false,
          t("toolbar.screenshot.title"),
        )}
      </div>
      <div className="flex min-w-0 flex-wrap items-center gap-x-3 gap-y-1 px-2 font-mono text-muted-foreground text-xs tabular-nums">
        {field("image", header.image)}
        {field("build", header.buildId)}
        {field("instance", header.instance)}
        {field("vt", `vt ${header.virtualTime}`)}
        {field("factor", header.realTimeFactor)}
        {field("lease", t("toolbar.lease", { owner: header.lease }))}
      </div>
      <div className="ms-auto min-w-0 px-2">
        <ToolbarNotice last={last} />
      </div>
    </div>
  );
}

function Panels() {
  const page = usePage();
  const t = useT();
  useStore(page.tabsVersion);
  return (
    <Card className="panels min-w-0 gap-0 overflow-hidden" render={<section aria-label={t("panels.label")} />}>
      <Tabs
        className="min-h-0 flex-1 gap-0"
        onValueChange={(value) => {
          page.actions.selectTab(String(value));
        }}
        value={page.tabs.active}
      >
        <div className="tab-strip overflow-x-auto overflow-y-hidden border-b px-2">
          <TabsList variant="underline">
            {TABS.map((tab) => (
              <TabsTab className="tab" id={`tab-${tab.id}`} key={tab.id} value={tab.id}>
                {t(`tab.${tab.id}`)}
              </TabsTab>
            ))}
          </TabsList>
        </div>
        {TABS.map((tab) => (
          <TabsPanel className="pane min-w-0 p-4" data-scroller="" id={`pane-${tab.id}`} keepMounted key={tab.id} value={tab.id}>
            {tab.id === "console" ? <ConsolePane /> : null}
            {tab.id === "ui-tree" ? <UiTreePane /> : null}
            {tab.id === "events" ? <EventsPane /> : null}
            {tab.id === "inspect" ? <InspectPane /> : null}
            {tab.id === "fidelity" ? <FidelityPane /> : null}
            {tab.id === "perf" ? <PerfPane /> : null}
          </TabsPanel>
        ))}
      </Tabs>
    </Card>
  );
}

const CARD_BODY: Record<CardSpec["id"], () => ReactNode> = {
  battery: () => <BatteryCard />,
  usb: () => <UsbCard />,
  audio: () => <AudioCard />,
  nfc: () => <NfcCard />,
  wifi: () => <WifiCard />,
  ble: () => <BleCard />,
  snapshots: () => <SnapshotsCard />,
};

export const MASONRY_ROW_PX = 2;

export const MASONRY_GAP_PX = 16;

export function masonrySpan(height: number): number {
  return Math.max(1, Math.ceil((height + MASONRY_GAP_PX) / MASONRY_ROW_PX));
}

/**
 * Masonry without reordering: grid rows are {@link MASONRY_ROW_PX} tall and each card spans as
 * many as its measured height needs, so DOM order (and tab order) is unchanged.
 */
function useMasonry() {
  const grid = useRef<HTMLDivElement>(null);
  useLayoutEffect(() => {
    const node = grid.current;
    if (!node) {
      return;
    }
    const place = (card: Element) => {
      if (card instanceof HTMLElement) {
        card.style.gridRowEnd = `span ${masonrySpan(card.getBoundingClientRect().height)}`;
      }
    };
    const cards = [...node.children];
    // Before the first paint: until then every card spans one row and they overlap.
    cards.forEach(place);
    if (typeof ResizeObserver === "undefined") {
      return;
    }
    const observer = new ResizeObserver((entries) => {
      for (const entry of entries) {
        place(entry.target);
      }
    });
    for (const card of cards) {
      observer.observe(card);
    }
    return () => {
      observer.disconnect();
    };
  }, []);
  return grid;
}

function Cards() {
  const page = usePage();
  const t = useT();
  useStore(page.tabsVersion);
  const narrow = useStore(page.layout).narrow;
  const grid = useMasonry();
  return (
    <div
      aria-label={t("cards.label")}
      className="cards grid auto-rows-[2px] gap-x-4 gap-y-0 sm:grid-cols-2 xl:grid-cols-3"
      ref={grid}
      role="region"
    >
      {CARDS.map((card) => {
        const collapsed = page.tabs.isCollapsed(card.id, narrow);
        return (
          <Card className={cn("card min-w-0 gap-0 self-start", collapsed && "collapsed")} data-card={card.id} key={card.id} render={<section />}>
            <button
              aria-controls={`card-${card.id}`}
              aria-expanded={collapsed ? "false" : "true"}
              className="card-toggle flex w-full items-center gap-2 rounded-2xl px-5 py-3.5 text-start font-semibold text-sm outline-none hover:bg-accent/40 focus-visible:ring-2 focus-visible:ring-ring"
              onClick={() => {
                page.actions.toggleCard(card.id);
              }}
              type="button"
            >
              {t(`card.${card.id}`)}
              <ChevronDownIcon
                aria-hidden="true"
                className={cn("ms-auto size-4 text-muted-foreground transition-transform", collapsed && "-rotate-90")}
              />
            </button>
            <div className="card-body flex flex-col gap-3 border-t px-5 pt-4 pb-5" hidden={collapsed} id={`card-${card.id}`}>
              {CARD_BODY[card.id]()}
            </div>
          </Card>
        );
      })}
    </div>
  );
}
