import { useRef, useState } from "react";
import type { SerialChannel } from "../../../api/commands";
import { cn } from "../../../ui/lib/utils";
import { usePage, useStore, useT } from "../hooks";
import { Select } from "../controls";
import { DeviceOutput, Steps } from "../Log";
import { FollowToggle, Terminal } from "../Terminal";

const SHELL =
  "relative inline-flex w-full min-w-0 rounded-lg border border-input bg-background text-base shadow-xs/5 ring-ring/24 transition-shadow has-focus-visible:border-ring has-focus-visible:ring-[3px] sm:text-sm dark:bg-input/32";
const INNER =
  "h-8.5 w-full min-w-0 rounded-[inherit] bg-transparent px-[calc(--spacing(3)-1px)] text-foreground outline-none placeholder:text-muted-foreground/72 sm:h-7.5";

export function ConsolePane() {
  const page = usePage();
  const t = useT();
  const version = useStore(page.consoleVersion);
  const [channel, setChannel] = useState<SerialChannel>("usj");
  const entry = useRef<HTMLInputElement>(null);
  const model = page.console;
  const steps = useStore(page.loader.store).steps;
  const follow = useStore(page.prefs).follow;
  const signal = (name: "dtr" | "rts", on: boolean) => (
    <span
      aria-pressed={on ? "true" : "false"}
      className={cn(
        "line-state rounded-md border px-1.5 py-0.5 font-mono text-[0.6875rem]",
        on ? "active border-foreground/40 bg-foreground text-background" : "text-muted-foreground",
      )}
      data-signal={name}
    >
      {name.toUpperCase()}
    </span>
  );
  return (
    <div className="console flex min-h-0 flex-1 flex-col gap-3">
      <div className="console-bar flex flex-wrap items-center gap-2">
        <span className={cn(SHELL, "min-w-40 flex-1")}>
          <input
            aria-label={t("console.filter.aria")}
            className={cn(INNER, "console-filter")}
            defaultValue={model.filterSource}
            onChange={(event) => {
              page.actions.setConsoleFilter(event.currentTarget.value);
            }}
            placeholder={t("console.filter")}
            type="search"
          />
        </span>
        {signal("dtr", model.state.dtr)}
        {signal("rts", model.state.rts)}
        <label className="inline-flex items-center gap-1.5 text-muted-foreground text-sm" htmlFor="console-capture">
          <input
            className="size-4 accent-foreground"
            defaultChecked={model.captureMode}
            id="console-capture"
            onChange={(event) => {
              page.actions.setCapture(event.currentTarget.checked);
            }}
            type="checkbox"
          />
          {t("console.capture")}
        </label>
        <FollowToggle checked={follow} id="console-follow" onChange={page.actions.setFollow} />
      </div>
      {steps.length === 0 ? null : <Steps steps={steps} t={t} />}
      <DeviceOutput
        className="h-[26rem] max-h-[60dvh] min-[900px]:h-0 min-[900px]:max-h-none min-[900px]:min-h-48 min-[900px]:flex-1 dark:bg-black/25"
        hint={t("console.deviceHint")}
        t={t}
      >
        <Terminal
          className="min-h-0 flex-1 pb-2"
          follow={follow}
          label={t("console.aria")}
          lines={model.visible()}
          tagStream={model.streamsSeen().length > 1}
          version={version}
        />
      </DeviceOutput>
      <div className="console-entry flex items-center gap-2">
        <Select
          ariaLabel={t("console.channel.aria")}
          className="w-24 shrink-0"
          onChange={setChannel}
          options={[
            { value: "usj", label: "usj" },
            { value: "uart0", label: "uart0" },
          ]}
          value={channel}
        />
        <span className={cn(SHELL, "flex-1")}>
          <input
            aria-label={t("console.input.aria")}
            className={cn(INNER, "console-input font-mono")}
            onKeyDown={(event) => {
              const node = event.currentTarget;
              if (event.key !== "Enter" || node.value.length === 0) {
                return;
              }
              event.preventDefault();
              // The newline is part of what is sent: the guest's line readers block until one arrives.
              page.actions.serialWrite(channel, `${node.value}\n`);
              node.value = "";
            }}
            placeholder={t("console.input")}
            ref={entry}
            type="text"
          />
        </span>
      </div>
    </div>
  );
}
