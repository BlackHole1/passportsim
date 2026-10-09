// Simple mode's log: the page's own steps and the device's serial output as two captioned blocks,
// since a legend did not tell them apart. Page lines are steps that happened; a download's line
// counts its bytes in place.

import { TerminalSquareIcon } from "lucide-react";
import { useLayoutEffect, useRef, type ReactNode } from "react";
import { Card } from "../../ui/card";
import { cn } from "../../ui/lib/utils";
import { downloadText } from "../download";
import type { Translate } from "../i18n";
import { humanSize } from "../load";
import type { ProgressLine, ProgressStep } from "../loader";
import { PLAY_HOST } from "../play";
import { stopReasonKey, type MachineStop } from "../stop";
import { usePage, useStore, useT } from "./hooks";
import { FollowToggle, Terminal } from "./Terminal";

export function stepText(t: Translate, step: ProgressStep): string {
  switch (step.kind) {
    case "demo":
      return t("step.demo");
    case "no-demo":
      return t("step.noDemo");
    case "history":
      return t("step.history", { name: step.name });
    case "received":
      return t("step.received", { files: step.files, size: humanSize(step.bytes) });
    case "detected":
      switch (step.what) {
        case "bundle":
          return t("step.detected.bundle");
        case "merged-bin":
          return t("step.detected.merged");
        case "elf":
          return t("step.detected.elf");
        case "build-directory":
          return t("step.detected.build");
        case "files":
          return t("step.detected.files");
      }
      break;
    case "read":
      return t("step.read", { path: step.path, size: humanSize(step.bytes) });
    case "assembled":
      return t("step.assembled", { size: humanSize(step.bytes), list: step.list, parts: step.parts });
    case "boot":
      return t("step.boot", { name: step.name });
    case "ready":
      return t("step.ready", { name: step.name });
    case "console":
      return t("step.console");
    case "failed":
      return t("step.failed");
    case "stopped":
      return t("step.stopped", { vt: step.vt, reason: stopReason(t, step.stop), name: step.stop.name });
    case "machine-error":
      return t("step.machineError", { detail: step.detail });
    case "refused":
      return t("step.refused", { command: step.command, error: step.error });
    case "download":
      return downloadText(t, step.progress);
    case "play":
      return t("step.play", { host: PLAY_HOST, id: step.id });
    case "play-found":
      return step.title === ""
        ? t("step.playFound", { id: step.id, revision: step.revision, size: humanSize(step.bytes) })
        : t("step.playFoundTitled", { id: step.id, title: step.title, revision: step.revision, size: humanSize(step.bytes) });
    case "play-verified":
      return t("step.playVerified", { sha256: step.sha256.slice(0, 12) });
  }
  return "";
}

export function stopReason(t: Translate, stop: MachineStop): string {
  return t(stopReasonKey(stop.code), { code: stop.code });
}

function PageLine(props: { readonly line: ProgressLine; readonly t: Translate }) {
  const { line, t } = props;
  return (
    <div className="log-page flex gap-2 text-foreground" data-step={line.step.kind}>
      <span className="shrink-0 select-none font-mono text-[0.6875rem] text-muted-foreground tabular-nums leading-5">{`${`+${line.atMs.toFixed(0)}`.padStart(6, " ")} ms`}</span>
      <span>
        <span aria-hidden="true" className="select-none text-muted-foreground">
          {"› "}
        </span>
        {stepText(t, line.step)}
      </span>
    </div>
  );
}

export function BlockCaption(props: { readonly title: string; readonly hint: string; readonly children?: ReactNode }) {
  return (
    <div className="block-caption flex min-w-0 items-baseline gap-2 font-sans">
      <span className="shrink-0 font-semibold text-[0.6875rem] text-foreground uppercase tracking-wide">{props.title}</span>
      <span className="min-w-0 text-[0.6875rem] text-muted-foreground">{props.hint}</span>
      {props.children === undefined ? null : <span className="ms-auto shrink-0">{props.children}</span>}
    </div>
  );
}

export function Steps(props: {
  readonly steps: readonly ProgressLine[];
  readonly t: Translate;
  readonly className?: string;
  readonly rows?: number;
}) {
  const box = useRef<HTMLDivElement>(null);
  useLayoutEffect(() => {
    const node = box.current;
    if (node) {
      node.scrollTop = node.scrollHeight;
    }
  }, [props.steps]);
  return (
    <section
      aria-label={props.t("log.steps")}
      className={cn("log-steps flex shrink-0 flex-col gap-1 rounded-lg border border-dashed bg-background px-3 py-2 text-xs", props.className)}
    >
      <BlockCaption hint={props.t("log.stepsHint")} title={props.t("log.steps")} />
      <div className="overflow-y-auto leading-5" data-scroller="" ref={box} style={{ maxHeight: `${1.25 * (props.rows ?? 5)}rem` }}>
        {props.steps.map((line) => (
          <PageLine key={`step-${line.seq}`} line={line} t={props.t} />
        ))}
      </div>
    </section>
  );
}

export function DeviceOutput(props: {
  readonly className?: string;
  readonly children: ReactNode;
  readonly t: Translate;
  readonly hint: string;
}) {
  return (
    <section
      aria-label={props.t("log.device")}
      className={cn("log-device flex min-h-0 flex-col gap-1 rounded-lg border bg-muted/40 px-3 pt-2 dark:bg-black/20", props.className)}
    >
      <BlockCaption hint={props.hint} title={props.t("log.device")} />
      {props.children}
    </section>
  );
}

export function Log() {
  const page = usePage();
  const t = useT();
  const version = useStore(page.consoleVersion);
  const loader = useStore(page.loader.store);
  const follow = useStore(page.prefs).follow;
  const lines = page.console.primary();
  return (
    <Card aria-label={t("log.title")} className="log-panel min-h-72 gap-0 overflow-hidden" render={<section />}>
      <div className="flex items-center gap-2 border-b px-5 py-3">
        <TerminalSquareIcon aria-hidden="true" className="size-4 text-muted-foreground" />
        <h2 className="font-semibold text-sm">{t("log.title")}</h2>
        <span className="ms-auto">
          <FollowToggle checked={follow} id="log-follow" onChange={page.actions.setFollow} />
        </span>
      </div>
      <div className="log-body flex min-h-0 flex-1 flex-col gap-3 p-3">
        {loader.steps.length === 0 ? null : <Steps rows={3} steps={loader.steps} t={t} />}
        <DeviceOutput className="flex-1" hint={t("log.deviceHint")} t={t}>
          <Terminal className="min-h-0 flex-1 pb-2" follow={follow} label={t("log.device")} lines={lines} rows={400} version={version} />
        </DeviceOutput>
      </div>
    </Card>
  );
}
