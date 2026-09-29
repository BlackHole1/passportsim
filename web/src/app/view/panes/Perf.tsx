import * as perf from "../../panels/perf";
import type { Translate } from "../../i18n";
import { usePage, useStore, useT } from "../hooks";

function clockText(t: Translate, granularityMs: number): string {
  const base = granularityMs <= 0 ? t("perf.clockUnmeasured") : t("perf.clock", { ms: granularityMs.toFixed(1) });
  return perf.isMeaningful(perf.WINDOW_MS, granularityMs) ? base : `${base}; ${t("perf.tooShort")}`;
}

export function PerfPane() {
  const page = usePage();
  const t = useT();
  useStore(page.perfVersion);
  const report = page.perf.report();
  const rows: [string, string][] = [
    [t("perf.factor"), report.current.toFixed(2)],
    [t("perf.worst"), report.worst.toFixed(2)],
    [t("perf.median"), report.median.toFixed(2)],
    [t("perf.vtPerSecond"), report.virtualMsPerSecond.toFixed(0)],
    [t("perf.reanchors"), String(report.reanchors)],
    [t("perf.windows"), String(report.samples.length)],
  ];
  return (
    <div className="flex max-w-xl flex-col gap-3">
      <div className="overflow-hidden rounded-lg border">
        <table className="w-full text-sm">
          <tbody className="[&>tr+tr]:border-t">
            {rows.map(([label, value]) => (
              <tr key={label}>
                <th className="px-3 py-1.5 text-left font-normal text-muted-foreground">{label}</th>
                <td className="px-3 py-1.5 text-right font-mono tabular-nums">{value}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      <p className="card-note text-muted-foreground text-xs">{clockText(t, report.clockGranularityMs)}</p>
    </div>
  );
}
