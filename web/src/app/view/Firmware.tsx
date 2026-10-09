// The firmware panel: the drop zone, both file inputs and what the loader says. One element in
// both modes, so the `data-loader-*` attributes Playwright reads exist once.

import {
  AlertCircleIcon,
  CloudDownloadIcon,
  ExternalLinkIcon,
  FileUpIcon,
  FolderOpenIcon,
  ImageDownIcon,
  MonitorSmartphoneIcon,
  RotateCcwIcon,
  UploadIcon,
} from "lucide-react";
import { useRef } from "react";
import { Button } from "../../ui/button";
import { Card } from "../../ui/card";
import { cn } from "../../ui/lib/utils";
import { Spinner } from "../../ui/spinner";
import { downloadText } from "../download";
import { dropFromFiles } from "../drop";
import type { Translate } from "../i18n";
import { DEMO_IMAGE, FLASH_SIZE_BYTES, humanSize } from "../load";
import { runsNothing, type LoaderMessage } from "../loader";
import { PLAY_FORMAT, PLAY_HOST, playPageUrl, type PlayFault, type PlayRefFault } from "../play";
import { INPUT_INNER, INPUT_SHELL } from "./controls";
import { HistoryDialog } from "./History";
import { usePage, useStore, useT } from "./hooks";
import { ElfHint } from "./notices";

/** A play the page's own text names, so the placeholder and a refusal show the same one. */
const EXAMPLE_PLAY = 22;

/** Why a typed text names no play. */
export function playRefFaultText(t: Translate, fault: PlayRefFault): string {
  switch (fault.kind) {
    case "empty":
      return t("play.fault.empty", { host: PLAY_HOST });
    case "other-site":
      return t("play.fault.otherSite", { other: fault.host, host: PLAY_HOST });
    case "not-a-play":
      return t("play.fault.notAPlay", { example: playPageUrl(EXAMPLE_PLAY) });
  }
}

/** Why the play site gave no firmware the page loads for play `id`. */
export function playFaultText(t: Translate, id: number, fault: PlayFault): string {
  const host = PLAY_HOST;
  switch (fault.kind) {
    case "unreachable":
      return t("play.fault.unreachable", { id, detail: fault.detail });
    case "no-relay":
      return t("play.fault.noRelay", { host });
    case "not-found":
      return t("play.fault.notFound", { host, id: fault.id });
    case "http":
      return t("play.fault.http", { host, id, status: fault.status });
    case "malformed":
      return t("play.fault.malformed", { host, id, detail: fault.detail });
    case "no-firmware":
      return t("play.fault.noFirmware", { id: fault.id });
    case "format":
      return t("play.fault.format", { id, format: fault.format, expected: PLAY_FORMAT });
    case "too-large":
      return t("play.fault.tooLarge", { id, size: humanSize(fault.size), limit: humanSize(FLASH_SIZE_BYTES) });
    case "size":
      return t("play.fault.size", { host, received: fault.received, stated: fault.stated });
    case "sha256":
      return t("play.fault.sha256", { host, computed: fault.computed, stated: fault.stated });
    case "not-an-image":
      return t("play.fault.notAnImage", { host });
  }
}

export function loaderText(t: Translate, message: LoaderMessage | null): string {
  if (message === null) {
    return "";
  }
  switch (message.kind) {
    case "reading":
      return t("loader.reading");
    case "unreadable":
      return t("loader.unreadable", { detail: message.detail });
    case "refused":
      return message.reason;
    case "booting":
      return t("loader.booting", { name: message.name, notes: message.notes.join("; ") });
    case "not-booted":
      return t("loader.notBooted", { name: message.name, detail: message.detail });
    case "running":
      return message.notes.length > 0
        ? t("loader.running", { name: message.name, notes: message.notes.join("; ") })
        : t("loader.runningBare", { name: message.name });
    case "machine":
      return message.detail;
    case "no-demo":
      return t("loader.noDemo");
    case "play-fetching":
      return message.progress === null ? t("loader.playFetching", { host: PLAY_HOST, id: message.id }) : downloadText(t, message.progress);
    case "play-ref":
      return playRefFaultText(t, message.fault);
    case "play-fault":
      return playFaultText(t, message.id, message.fault);
  }
}

/**
 * The play box: a play's link or number, loaded from the play site through the relay on the page's
 * own origin. An uncontrolled input read on submit, like the cards' fields. The hint names the
 * site, since this is the one thing on the page that asks its server for anything but its files.
 */
function PlayForm(props: { readonly compact: boolean; readonly disabled: boolean }) {
  const page = usePage();
  const t = useT();
  const input = useRef<HTMLInputElement>(null);
  return (
    <form
      className={cn("play-form flex min-w-0 flex-col", props.compact ? "gap-1" : "gap-1.5")}
      data-play-form=""
      onSubmit={(event) => {
        event.preventDefault();
        void page.actions.loadPlay(input.current?.value ?? "");
      }}
    >
      <label className={cn("text-muted-foreground", props.compact ? "text-xs" : "text-sm")} htmlFor="play-ref">
        {t("play.label", { host: PLAY_HOST })}
      </label>
      <div className="flex min-w-0 items-center gap-2">
        <span className={INPUT_SHELL}>
          <input
            autoCapitalize="off"
            autoComplete="off"
            className={INPUT_INNER}
            data-play-input=""
            id="play-ref"
            inputMode="url"
            placeholder={playPageUrl(EXAMPLE_PLAY)}
            ref={input}
            spellCheck={false}
            type="text"
          />
        </span>
        <Button className="shrink-0" data-action="load-play" disabled={props.disabled} size={props.compact ? "sm" : "default"} type="submit" variant="outline">
          <CloudDownloadIcon aria-hidden="true" />
          {t("play.load")}
        </Button>
      </div>
      <p className="text-muted-foreground text-xs leading-relaxed">{t("play.hint", { host: PLAY_HOST })}</p>
    </form>
  );
}

/** The way out of a play the page could not fetch: its page on the play site, opened by hand. */
function PlayPageLink(props: { readonly message: LoaderMessage | null }) {
  const t = useT();
  const { message } = props;
  if (message?.kind !== "play-fault") {
    return null;
  }
  return (
    <a
      className="inline-flex items-center gap-1 self-start text-xs underline underline-offset-2"
      data-play-page=""
      href={playPageUrl(message.id)}
      rel="noreferrer"
      target="_blank"
    >
      {t("play.openPage", { host: PLAY_HOST, id: message.id })}
      <ExternalLinkIcon aria-hidden="true" className="size-3" />
    </a>
  );
}

/**
 * The page's one sentence about where things go: nowhere. The deployment's only server code is the
 * play box's relay (`edge/playRelay.js`), which is sent a play's number and nothing of the page's.
 */
export function StaysHere(props: { readonly className?: string }) {
  const t = useT();
  return (
    <p className={cn("flex items-start gap-1.5 text-muted-foreground text-xs leading-relaxed", props.className)} data-stays-here="">
      <MonitorSmartphoneIcon aria-hidden="true" className="mt-px size-3.5 shrink-0" />
      <span>{t("firmware.staysHere")}</span>
    </p>
  );
}

function ScreenshotButton() {
  const page = usePage();
  const t = useT();
  return (
    <Button
      data-action="screenshot"
      onClick={() => {
        void page.actions.screenshot();
      }}
      size="sm"
      title={t("toolbar.screenshot.title")}
      variant="ghost"
    >
      <ImageDownIcon aria-hidden="true" />
      {t("toolbar.screenshot")}
    </Button>
  );
}

function ShotNotice() {
  const page = usePage();
  const t = useT();
  const shot = useStore(page.capture);
  const mode = useStore(page.prefs).mode;
  if (shot === null || mode !== "simple") {
    return null;
  }
  return (
    <p
      className={cn("text-xs", shot.kind === "failed" ? "text-destructive-foreground" : "text-muted-foreground")}
      data-notice={shot.kind === "failed" ? "shot-failed" : "shot"}
      role="status"
    >
      {shot.kind === "failed" ? t("toolbar.shotFailed", { reason: shot.reason }) : t("toolbar.shotSaved", { file: shot.file })}
    </p>
  );
}

export function Firmware() {
  const page = usePage();
  const t = useT();
  const mode = useStore(page.prefs).mode;
  const loader = useStore(page.loader.store);
  const boot = useStore(page.boot);
  const files = useRef<HTMLInputElement>(null);
  const directory = useRef<HTMLInputElement>(null);

  const fromInput = (input: HTMLInputElement | null) => {
    const picked = input?.files;
    if (!input || !picked || picked.length === 0) {
      return;
    }
    void page.loader.offer(dropFromFiles(picked));
    // Cleared so that picking the same path twice fires `change` again.
    input.value = "";
  };

  const failed = loader.state === "refused" || loader.state === "error";
  const empty = runsNothing(loader);
  const loading = loader.state === "loading";
  const onDemo = loader.image === DEMO_IMAGE;
  const text = loaderText(t, loader.message);
  const simple = mode === "simple";
  const noElf = !loader.elf && !loading;

  const inputs = (
    <>
      <input
        aria-label={t("firmware.chooseFiles")}
        className="sr-only"
        data-loader-input="files"
        id="loader-files"
        multiple
        onChange={(event) => {
          fromInput(event.currentTarget);
        }}
        ref={files}
        tabIndex={-1}
        type="file"
      />
      <input
        aria-label={t("firmware.chooseFolder")}
        className="sr-only"
        data-loader-input="directory"
        id="loader-directory"
        onChange={(event) => {
          fromInput(event.currentTarget);
        }}
        ref={directory}
        tabIndex={-1}
        type="file"
        // Not a React or TypeScript attribute.
        {...{ webkitdirectory: "" }}
      />
    </>
  );
  const pickers = (size: "sm" | "default") => (
    <>
      <Button
        onClick={() => {
          files.current?.click();
        }}
        size={size}
      >
        <FileUpIcon aria-hidden="true" />
        {t("firmware.chooseFiles")}
      </Button>
      <Button
        onClick={() => {
          directory.current?.click();
        }}
        size={size}
        variant="outline"
      >
        <FolderOpenIcon aria-hidden="true" />
        {t("firmware.chooseFolder")}
      </Button>
    </>
  );
  const backToDemo = !loader.demo ? null : (
    <Button
      data-action="back-to-demo"
      disabled={loading || (onDemo && !failed)}
      onClick={() => {
        void page.loader.backToDemo();
      }}
      size="sm"
      variant="ghost"
    >
      <RotateCcwIcon aria-hidden="true" />
      {t("firmware.backToDemo")}
    </Button>
  );
  const message = (
    <p
      className={cn(
        "break-words text-xs empty:hidden",
        failed ? "text-destructive-foreground" : "text-muted-foreground",
        !simple && "font-mono",
      )}
      data-loader-message=""
      role="status"
    >
      {text}
    </p>
  );
  const root = {
    "aria-label": t("firmware.title"),
    "data-loader": "",
    "data-loader-image": loader.image,
    "data-loader-state": loader.state,
  };

  if (!simple) {
    return (
      <section {...root} className="firmware firmware-strip flex min-w-0 flex-col gap-2 rounded-xl border bg-card px-4 py-3 shadow-xs/5">
        <div className="flex flex-wrap items-center gap-2">
          <UploadIcon aria-hidden="true" className="size-4 text-muted-foreground" />
          <span className="me-auto text-muted-foreground text-sm">{t("firmware.dropAnywhere")}</span>
          {pickers("sm")}
          {backToDemo}
        </div>
        <div className="flex flex-wrap items-center gap-x-2">
          <StaysHere className="me-auto" />
          <HistoryDialog size="sm" />
        </div>
        <PlayForm compact disabled={loading} />
        {message}
        <PlayPageLink message={loader.message} />
        {noElf ? <ElfHint /> : null}
        {inputs}
      </section>
    );
  }

  return (
    <Card {...root} className="firmware firmware-card gap-5 p-5 sm:p-6" render={<section />}>
      <div className="flex flex-col gap-1">
        <div className="flex flex-wrap items-center gap-x-2">
          <h2 className="me-auto font-semibold text-base leading-tight">{t("firmware.title")}</h2>
          <div className="-my-1.5 -me-2.5 flex items-center">
            <HistoryDialog size="sm" />
            <ScreenshotButton />
          </div>
        </div>
        <p className="text-muted-foreground text-sm">{t("firmware.subtitle")}</p>
      </div>
      <div
        className={cn(
          "drop-zone flex flex-col items-center gap-3 rounded-xl border-2 border-dashed px-4 py-7 text-center transition-colors",
          loader.over ? "border-foreground/48 bg-accent" : "border-input",
        )}
        data-over={loader.over ? "true" : "false"}
      >
        <span className="flex size-11 items-center justify-center rounded-full bg-muted text-muted-foreground">
          <UploadIcon aria-hidden="true" className="size-5" />
        </span>
        <div className="flex flex-col gap-1">
          <p className="font-medium text-sm">{t("firmware.drop")}</p>
          <p className="text-muted-foreground text-xs leading-relaxed">{t("firmware.accepts")}</p>
        </div>
        <div className="flex flex-wrap justify-center gap-2">{pickers("default")}</div>
      </div>
      <StaysHere className="-mt-2" />
      <PlayForm compact={false} disabled={loading} />
      <div className="flex flex-col gap-2">
        <div className="flex flex-wrap items-center gap-x-2 gap-y-1">
          <span className="text-muted-foreground text-xs uppercase tracking-wide">{t("firmware.current")}</span>
          <span className={cn("min-w-24 flex-1 truncate text-sm", empty ? "text-muted-foreground" : "font-medium")} title={loader.image}>
            {empty ? t("firmware.none") : onDemo ? t("status.demo") : loader.image}
          </span>
          {loading || (boot.starting && boot.failure === null && !empty) ? <Spinner aria-label={t("firmware.loading")} className="size-4 text-muted-foreground" /> : null}
          {backToDemo}
        </div>
        {failed ? (
          <div className="flex gap-2.5 rounded-lg border border-destructive/32 bg-destructive/4 px-3 py-2.5" role="alert">
            <AlertCircleIcon aria-hidden="true" className="mt-0.5 size-4 shrink-0 text-destructive" />
            <div className="flex min-w-0 flex-col gap-1">
              <p className="font-medium text-sm">{t(loader.message?.kind === "machine" ? "firmware.machineError" : "firmware.failed")}</p>
              {message}
              <PlayPageLink message={loader.message} />
              {/* A refusal comes before the machine is replaced; a boot the Worker refused comes after. */}
              {loader.state === "refused" && !empty ? <p className="text-muted-foreground text-xs">{t("firmware.kept")}</p> : null}
            </div>
          </div>
        ) : (
          message
        )}
        {noElf ? <ElfHint /> : null}
        <ShotNotice />
      </div>
      {inputs}
    </Card>
  );
}
