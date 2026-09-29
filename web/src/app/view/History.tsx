// The firmware history dialog. Everything in it is read from this browser's storage.

import { Dialog } from "@base-ui/react/dialog";
import { DownloadIcon, HistoryIcon, ImageOffIcon, PlayIcon, Trash2Icon, XIcon } from "lucide-react";
import { useMemo, useState } from "react";
import { Button } from "../../ui/button";
import { cn } from "../../ui/lib/utils";
import type { HistoryEntry, NotKept } from "../history";
import { MAX_ENTRIES, MAX_TOTAL_BYTES } from "../history";
import type { Translate } from "../i18n";
import { humanSize } from "../load";
import { usePage, useStore, useT } from "./hooks";

export function notKeptText(t: Translate, notKept: NotKept): string {
  switch (notKept.kind) {
    case "too-large":
      return t("history.notKept.large", { name: notKept.name, size: humanSize(notKept.size), limit: humanSize(MAX_TOTAL_BYTES) });
    case "device-backup":
      return t("history.notKept.backup", { name: notKept.name });
    case "quota":
      return t("history.notKept.quota", { name: notKept.name, detail: notKept.detail });
  }
}

function pngUrl(bytes: Uint8Array | null): string | null {
  if (bytes === null) {
    return null;
  }
  let text = "";
  for (let i = 0; i < bytes.length; i += 0x8000) {
    text += String.fromCharCode(...bytes.subarray(i, i + 0x8000));
  }
  return `data:image/png;base64,${btoa(text)}`;
}

function Entry(props: { readonly entry: HistoryEntry; readonly onLoaded: () => void }) {
  const page = usePage();
  const t = useT();
  const locale = useStore(page.prefs).locale;
  const loading = useStore(page.loader.store).state === "loading";
  const { entry } = props;
  const thumbnail = useMemo(() => pngUrl(entry.thumbnail), [entry.thumbnail]);
  const when = useMemo(() => {
    try {
      return new Intl.DateTimeFormat(locale, { dateStyle: "medium", timeStyle: "short" }).format(entry.lastLoadedAt);
    } catch {
      return new Date(entry.lastLoadedAt).toISOString();
    }
  }, [entry.lastLoadedAt, locale]);
  const meta = [
    humanSize(entry.size),
    t("history.build", { id: entry.buildId ?? "--" }),
    entry.project === null ? null : [entry.project, entry.version].filter((part) => part !== null).join(" "),
  ].filter((part) => part !== null);
  return (
    <li className="history-entry flex gap-3 border-t py-3 first:border-t-0" data-history-entry={entry.id}>
      <div className="flex h-20 w-15 shrink-0 items-center justify-center overflow-hidden rounded-md border bg-muted">
        {thumbnail === null ? (
          <ImageOffIcon aria-label={t("history.noPicture")} className="size-4 text-muted-foreground" />
        ) : (
          <img alt={t("history.picture", { name: entry.name })} className="size-full object-cover" data-history-thumbnail="" src={thumbnail} />
        )}
      </div>
      <div className="flex min-w-0 flex-1 flex-col gap-1">
        <p className="truncate font-medium text-sm" title={entry.name}>
          {entry.name}
        </p>
        <p className="font-mono text-muted-foreground text-xs [overflow-wrap:anywhere]">{meta.join(" | ")}</p>
        <p className="text-muted-foreground text-xs">{t("history.loadedAt", { when })}</p>
        <div className="mt-1 flex flex-wrap gap-1.5">
          <Button
            data-history-action="load"
            disabled={loading}
            onClick={() => {
              props.onLoaded();
              void page.actions.loadFromHistory(entry.id);
            }}
            size="xs"
          >
            <PlayIcon aria-hidden="true" />
            {t("history.load")}
          </Button>
          {entry.files.map((file) => (
            <Button
              data-history-action="download"
              data-history-file={file.name}
              key={file.name}
              onClick={() => {
                void page.actions.downloadFromHistory(entry.id, file.name);
              }}
              size="xs"
              title={t("history.download.title", { file: file.name, size: humanSize(file.size) })}
              variant="outline"
            >
              <DownloadIcon aria-hidden="true" />
              {entry.files.length === 1 ? t("history.download") : file.name}
            </Button>
          ))}
          <Button
            aria-label={t("history.delete.aria", { name: entry.name })}
            data-history-action="delete"
            onClick={() => {
              void page.history.remove(entry.id);
            }}
            size="xs"
            variant="ghost"
          >
            <Trash2Icon aria-hidden="true" />
            {t("history.delete")}
          </Button>
        </div>
      </div>
    </li>
  );
}

export function HistoryDialog(props: { readonly size: "xs" | "sm" }) {
  const page = usePage();
  const t = useT();
  const view = useStore(page.history.store);
  const [open, setOpen] = useState(false);
  const count = view.entries.length;
  return (
    <Dialog.Root onOpenChange={setOpen} open={open}>
      <Dialog.Trigger
        render={
          <Button data-action="history" size={props.size} variant="ghost">
            <HistoryIcon aria-hidden="true" />
            {count > 0 ? t("history.buttonCount", { count }) : t("history.button")}
          </Button>
        }
      />
      {/* Inside the page's root, so a firmware dropped on the dialog reaches the loader like any drop. */}
      <Dialog.Portal container={typeof document === "undefined" ? undefined : document.querySelector<HTMLElement>(".app")}>
        <Dialog.Backdrop className="fixed inset-0 z-50 bg-black/32 backdrop-blur-[1px]" />
        <Dialog.Popup
          className="fixed top-1/2 left-1/2 z-50 flex max-h-[min(40rem,calc(100dvh-2rem))] w-[min(36rem,calc(100vw-2rem))] -translate-x-1/2 -translate-y-1/2 flex-col gap-3 rounded-2xl border bg-popover p-5 text-popover-foreground shadow-lg/5 outline-none"
          data-history=""
          data-history-state={view.state}
        >
          <div className="flex items-start gap-3">
            <div className="flex min-w-0 flex-1 flex-col gap-1">
              <Dialog.Title className="font-semibold text-base">{t("history.title")}</Dialog.Title>
              <Dialog.Description className="text-muted-foreground text-xs leading-relaxed">
                {t("history.description", { count: MAX_ENTRIES, size: humanSize(MAX_TOTAL_BYTES) })}
              </Dialog.Description>
            </div>
            <Dialog.Close
              aria-label={t("history.close")}
              className="rounded-md p-1 text-muted-foreground outline-none hover:bg-accent focus-visible:ring-2 focus-visible:ring-ring"
            >
              <XIcon aria-hidden="true" className="size-4" />
            </Dialog.Close>
          </div>
          <div className="min-h-0 flex-1 overflow-y-auto" data-scroller="">
            {view.state === "opening" ? <p className="text-muted-foreground text-sm">{t("history.opening")}</p> : null}
            {view.state === "unavailable" ? (
              <p className="rounded-lg border border-dashed px-3 py-2.5 text-sm" data-history-unavailable="">
                {t("history.unavailable", { reason: view.reason ?? "" })}
              </p>
            ) : null}
            {view.state === "ready" && count === 0 ? <p className="text-muted-foreground text-sm">{t("history.empty")}</p> : null}
            {count > 0 ? (
              <ul className="flex flex-col">
                {view.entries.map((entry) => (
                  <Entry
                    entry={entry}
                    key={entry.id}
                    onLoaded={() => {
                      setOpen(false);
                    }}
                  />
                ))}
              </ul>
            ) : null}
          </div>
          {view.notKept === null ? null : (
            <p className="text-muted-foreground text-xs" data-history-not-kept={view.notKept.kind}>
              {notKeptText(t, view.notKept)}
            </p>
          )}
          {view.error === null ? null : <p className="text-destructive-foreground text-xs">{t("history.error", { error: view.error })}</p>}
          <div className={cn("flex items-center gap-2 border-t pt-3", count === 0 && "hidden")}>
            <Button
              data-history-action="clear"
              onClick={() => {
                if (page.ctx.confirm(t("history.clear.confirm", { count }))) {
                  void page.history.clear();
                }
              }}
              size="sm"
              variant="destructive-outline"
            >
              {t("history.clear")}
            </Button>
          </div>
        </Dialog.Popup>
      </Dialog.Portal>
    </Dialog.Root>
  );
}
