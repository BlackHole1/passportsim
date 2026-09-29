// The firmware panel: the drop zone, both file inputs and what the loader says. One element in
// both modes, so the `data-loader-*` attributes Playwright reads exist once.

import { AlertCircleIcon, FileUpIcon, FolderOpenIcon, ImageDownIcon, MonitorSmartphoneIcon, RotateCcwIcon, UploadIcon } from "lucide-react";
import { useRef } from "react";
import { Button } from "../../ui/button";
import { Card } from "../../ui/card";
import { cn } from "../../ui/lib/utils";
import { Spinner } from "../../ui/spinner";
import { dropFromFiles } from "../drop";
import type { Translate } from "../i18n";
import { DEMO_IMAGE } from "../load";
import { runsNothing, type LoaderMessage } from "../loader";
import { HistoryDialog } from "./History";
import { usePage, useStore, useT } from "./hooks";
import { ElfHint } from "./notices";

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
  }
}

/** The page's one sentence about where things go: nowhere. The deployment has no server code. */
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
        {message}
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
      <div className="flex flex-col gap-2">
        <div className="flex flex-wrap items-center gap-x-2 gap-y-1">
          <span className="text-muted-foreground text-xs uppercase tracking-wide">{t("firmware.current")}</span>
          <span className={cn("min-w-24 flex-1 truncate text-sm", empty ? "text-muted-foreground" : "font-medium")} title={loader.image}>
            {empty ? t("firmware.none") : onDemo ? t("status.demo") : loader.image}
          </span>
          {loading ? <Spinner aria-label={t("firmware.loading")} className="size-4 text-muted-foreground" /> : null}
          {backToDemo}
        </div>
        {failed ? (
          <div className="flex gap-2.5 rounded-lg border border-destructive/32 bg-destructive/4 px-3 py-2.5" role="alert">
            <AlertCircleIcon aria-hidden="true" className="mt-0.5 size-4 shrink-0 text-destructive" />
            <div className="flex min-w-0 flex-col gap-1">
              <p className="font-medium text-sm">{t(loader.message?.kind === "machine" ? "firmware.machineError" : "firmware.failed")}</p>
              {message}
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
