// Exported text is shown in the pane as well as copied, because outside a secure context there is
// no clipboard, and a silent failed copy would read as one that worked.

import { useState } from "react";
import type { Shell } from "../../../api/argv";
import type { Copied } from "../../../api/copy";
import { Button } from "../../../ui/button";
import { Select } from "../controls";
import { usePage, useStore, useT } from "../hooks";

export const VISIBLE_ROWS = 200;

export function EventsPane() {
  const page = usePage();
  const t = useT();
  useStore(page.eventsVersion);
  const actions = page.actions;
  const [shell, setShell] = useState<Shell>(actions.defaultShell);
  const [recording, setRecording] = useState(false);
  const [output, setOutput] = useState("");
  const [status, setStatus] = useState("");

  const show = async (result: Copied | { ok: true; text: string; note: string }) => {
    if (!result.ok) {
      setOutput("");
      setStatus(t("events.notCopied", { reason: result.reason }));
      return;
    }
    setOutput(result.text);
    const copied = await actions.writeClipboard(result.text);
    const parts = [copied ? t("events.copied") : t("events.shownBelow")];
    if ("redacted" in result && result.redacted.length > 0) {
      parts.push(t("events.redacted", { names: result.redacted.join(", ") }));
    }
    if ("note" in result && result.note !== "") {
      parts.push(result.note);
    }
    setStatus(parts.join("; "));
  };

  const rows = page.eventLog.list();
  const shown = rows.slice(Math.max(0, rows.length - VISIBLE_ROWS));
  const dropped = page.eventLog.dropped;
  const summary =
    rows.length === 0
      ? t("events.none")
      : [
          t("events.count", { count: rows.length }),
          dropped > 0 ? t("events.dropped", { count: dropped }) : null,
          shown.length < rows.length ? t("events.showing", { count: shown.length }) : null,
        ]
          .filter((part) => part !== null)
          .join(", ");

  return (
    <div className="flex flex-col gap-3">
      <div className="events-toolbar flex flex-wrap items-center gap-2">
        <label className="inline-flex items-center gap-2 text-muted-foreground text-sm">
          {t("events.shell")}
          <span>
            <Select
              ariaLabel={t("events.shell.aria")}
              className="w-36"
              data={{ "data-export": "shell" }}
              onChange={(value) => {
                setShell(value === "powershell" ? "powershell" : "sh");
              }}
              options={[
                { value: "sh", label: "sh" },
                { value: "powershell", label: "PowerShell" },
              ]}
              value={shell}
            />
          </span>
        </label>
        <Button
          aria-pressed={recording ? "true" : "false"}
          data-export="record"
          onClick={() => {
            if (recording) {
              setRecording(false);
              const out = actions.stopRecording();
              if (out !== null) {
                void show({
                  ok: true,
                  text: out.yaml,
                  note:
                    out.notes.length > 0
                      ? t("events.recordedNotes", { steps: out.steps, notes: out.notes.length })
                      : t("events.recorded", { steps: out.steps }),
                });
              }
              return;
            }
            actions.startRecording();
            setRecording(true);
            setStatus(t("events.recording"));
          }}
          size="sm"
          variant={recording ? "default" : "outline"}
        >
          {recording ? t("events.stop") : t("events.record")}
        </Button>
      </div>
      <div className="report overflow-x-auto overflow-y-hidden rounded-lg border">
        <table className="events w-full text-left text-xs">
          <thead className="bg-muted/60 text-muted-foreground">
            <tr className="[&>th]:px-2.5 [&>th]:py-1.5 [&>th]:font-medium">
              <th>vt</th>
              <th>{t("events.col.source")}</th>
              <th>{t("events.col.event")}</th>
              <th>{t("events.col.detail")}</th>
              <th>{t("events.col.copy")}</th>
            </tr>
          </thead>
          <tbody className="font-mono [&>tr]:border-t [&_td]:px-2.5 [&_td]:py-1 [&_td]:align-top">
            {shown.map((row) => {
              const seq = row.journalSeq;
              return (
                <tr data-seq={row.seq} data-source={row.source} key={row.seq}>
                  <td className="whitespace-nowrap tabular-nums">{row.vt}</td>
                  <td>{row.source}</td>
                  <td>{row.name}</td>
                  <td className="break-all">{row.detail}</td>
                  <td className="events-copy whitespace-nowrap">
                    {seq === null ? null : (
                      <span className="inline-flex gap-1">
                        <Button
                          aria-label={t("events.copyCli.aria", { name: row.name })}
                          data-copy="cli"
                          onClick={() => {
                            void show(actions.copyCli(seq, shell));
                          }}
                          size="xs"
                          variant="outline"
                        >
                          CLI
                        </Button>
                        <Button
                          aria-label={t("events.copyStep.aria", { name: row.name })}
                          data-copy="step"
                          onClick={() => {
                            void show(actions.copyStep(seq));
                          }}
                          size="xs"
                          variant="outline"
                        >
                          {t("events.step")}
                        </Button>
                      </span>
                    )}
                  </td>
                </tr>
              );
            })}
          </tbody>
        </table>
      </div>
      <p className="card-note text-muted-foreground text-xs">{summary}</p>
      <div className="events-export flex flex-col gap-2">
        <p aria-live="polite" className="card-note text-muted-foreground text-xs empty:hidden" data-export="status">
          {status}
        </p>
        <textarea
          aria-label={t("events.output.aria")}
          className="export-out min-h-28 w-full rounded-lg border border-input bg-background px-3 py-2 font-mono text-xs outline-none focus-visible:border-ring focus-visible:ring-[3px] focus-visible:ring-ring/24 dark:bg-input/32"
          data-export="output"
          readOnly
          rows={6}
          value={output}
        />
      </div>
    </div>
  );
}
