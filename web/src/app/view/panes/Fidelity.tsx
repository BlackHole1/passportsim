import { useState } from "react";
import * as fidelity from "../../panels/fidelity";
import { attempt } from "../../controllers";
import type { Translate } from "../../i18n";
import { cn } from "../../../ui/lib/utils";
import { Action, Actions, ErrorLine } from "../controls";
import { usePage, useT } from "../hooks";

// Neutral weights from validated to behavioural; only U, which blocks a strict pass, takes colour.
const CLASS_TONE: Record<fidelity.FidelityClass, string> = {
  A: "bg-foreground text-background",
  B: "bg-foreground/16 text-foreground",
  C: "border border-input text-foreground",
  U: "bg-destructive/10 text-destructive-foreground",
};

function verdictText(t: Translate, verdict: fidelity.StrictVerdict): string {
  if (!verdict.canPass) {
    return t("fidelity.blocked", { names: verdict.blocking.join(", ") });
  }
  return verdict.annotated.length === 0 ? t("fidelity.clean") : t("fidelity.caveats", { names: verdict.annotated.join(", ") });
}

export function FidelityPane() {
  const page = usePage();
  const t = useT();
  const [rows, setRows] = useState<readonly fidelity.FidelityRow[] | null>(null);
  const [verdict, setVerdict] = useState<fidelity.StrictVerdict | null>(null);
  const [error, setError] = useState<string | null>(null);
  const refresh = async () => {
    const done = await attempt(() => page.ctx.client.call("inspect", { what: ["fidelity"] }), false);
    if (!done.ok) {
      setError(done.error);
      return;
    }
    setRows(fidelity.byRisk(fidelity.parseRows(done.value.json)));
    setVerdict(fidelity.strictVerdict(done.value.receipt?.classes_touched ?? {}));
    setError(null);
  };
  return (
    <div className="flex flex-col gap-3">
      <Actions>
        <Action onClick={refresh} variant="default">
          {t("fidelity.refresh")}
        </Action>
      </Actions>
      {rows === null ? null : (
        <div className="report overflow-x-auto overflow-y-hidden rounded-lg border">
          <table className="w-full text-left text-xs">
            <thead className="bg-muted/60 text-muted-foreground">
              <tr className="[&>th]:px-2.5 [&>th]:py-1.5 [&>th]:font-medium">
                <th>{t("fidelity.col.subsystem")}</th>
                <th>{t("fidelity.col.class")}</th>
                <th>{t("fidelity.col.meaning")}</th>
              </tr>
            </thead>
            <tbody className="[&>tr]:border-t [&_td]:px-2.5 [&_td]:py-1.5">
              {rows.map((row) => (
                <tr data-class={row.class} key={row.subsystem}>
                  <td className="font-mono">{row.subsystem}</td>
                  <td>
                    <span className={cn("inline-flex min-w-6 justify-center rounded px-1.5 font-mono font-semibold", CLASS_TONE[row.class])}>
                      {row.class}
                    </span>
                  </td>
                  <td className="text-muted-foreground">{t(`fidelity.class.${row.class}`)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
      {verdict === null ? null : <p className="verdict text-sm">{verdictText(t, verdict)}</p>}
      <ErrorLine error={error} />
    </div>
  );
}
