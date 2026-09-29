// What the page says when the machine cannot do what it looks like it can: a stop, a missing ELF,
// an unbound radio.

import { CircleStopIcon, FileWarningIcon, PlayIcon, RotateCcwIcon } from "lucide-react";
import { useState } from "react";
import { CommandError } from "../../api/envelope";
import { Button } from "../../ui/button";
import { cn } from "../../ui/lib/utils";
import { isUnboundRadio, type Radio } from "../controllers";
import { formatVirtualTime } from "../header";
import { canContinue } from "../stop";
import { usePage, useStore, useT } from "./hooks";
import { stopReason } from "./Log";

export function ElfHint(props: { readonly className?: string }) {
  const t = useT();
  return (
    <div className={cn("elf-hint flex gap-2.5 rounded-lg border bg-muted/50 px-3 py-2.5", props.className)} data-elf-hint="" role="note">
      <FileWarningIcon aria-hidden="true" className="mt-0.5 size-4 shrink-0 text-muted-foreground" />
      <div className="flex min-w-0 flex-col gap-1 text-xs leading-relaxed">
        <p className="font-medium text-foreground text-sm">{t("elf.title")}</p>
        <p className="text-muted-foreground">{t("elf.body")}</p>
        <p className="text-muted-foreground">{t("elf.how")}</p>
      </div>
    </div>
  );
}

/**
 * A radio card's binding as its calls found it. `settle` maps a call's outcome to the card's error
 * line; an unbound radio is the card's state instead, with the raw refusal in the page's log.
 */
export function useRadioBinding(radio: Radio) {
  const page = usePage();
  const generation = useStore(page.generation);
  const [unboundAt, setUnboundAt] = useState<number | null>(null);
  const settle = (done: { ok: true } | { ok: false; error: string; cause: unknown }): string | null => {
    if (done.ok) {
      setUnboundAt(null);
      return null;
    }
    if (isUnboundRadio(done.cause, radio)) {
      setUnboundAt(generation);
      page.actions.logRefusal(done.cause instanceof CommandError ? done.cause.command : radio, done.error);
      return null;
    }
    return done.error;
  };
  return { unbound: unboundAt === generation, settle };
}

export function RadioUnbound(props: { readonly radio: Radio }) {
  const page = usePage();
  const t = useT();
  const elf = useStore(page.loader.store).elf;
  return (
    <div className="radio-unbound flex flex-col gap-2" data-radio-unbound={props.radio} role="status">
      <p className="font-medium text-sm">{t(props.radio === "ble" ? "radio.unbound.ble" : "radio.unbound.wifi")}</p>
      {elf ? null : <ElfHint />}
    </div>
  );
}

export function StopNotice(props: { readonly className?: string }) {
  const page = usePage();
  const t = useT();
  const stop = useStore(page.stop);
  if (stop === null) {
    return null;
  }
  return (
    <div
      className={cn("stop-notice flex w-full max-w-md cursor-auto flex-col gap-2.5 rounded-xl border border-destructive/32 bg-card px-4 py-3 shadow-xs/5", props.className)}
      data-stop={stop.name}
      role="alert"
    >
      <div className="flex items-start gap-2.5">
        <CircleStopIcon aria-hidden="true" className="mt-0.5 size-4 shrink-0 text-destructive" />
        <div className="flex min-w-0 flex-col gap-1">
          <p className="font-medium text-sm">{t("stop.title", { reason: stopReason(t, stop) })}</p>
          <p className="text-muted-foreground text-xs">
            {stop.vtPs === null ? null : `${t("stop.at", { vt: formatVirtualTime(stop.vtPs) })} `}
            {t("stop.input")}
          </p>
        </div>
      </div>
      <details className="group text-xs">
        <summary className="cursor-pointer select-none text-muted-foreground hover:text-foreground">{t("stop.detail")}</summary>
        <pre className="stop-detail mt-1.5 max-h-40 overflow-auto whitespace-pre-wrap rounded-md bg-muted px-2.5 py-2 font-mono text-[0.6875rem] leading-4 [overflow-wrap:anywhere]">
          {`${stop.name} (${stop.code})${stop.detail === null ? "" : `\n${stop.detail}`}`}
        </pre>
      </details>
      <div className="flex flex-wrap gap-2">
        {canContinue(stop) ? (
          <Button data-action="continue" onClick={page.actions.run} size="sm" variant="outline">
            <PlayIcon aria-hidden="true" />
            {t("stop.continue")}
          </Button>
        ) : null}
        <Button data-action="restart" onClick={page.actions.restart} size="sm" variant="outline">
          <RotateCcwIcon aria-hidden="true" />
          {t("stop.restart")}
        </Button>
      </div>
    </div>
  );
}
