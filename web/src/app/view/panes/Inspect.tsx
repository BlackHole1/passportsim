import { useState } from "react";
import * as inspect from "../../panels/inspect";
import { attempt } from "../../controllers";
import { Action, Actions, ErrorLine, Field, Note, Select, fieldId } from "../controls";
import { usePage, useT } from "../hooks";

export function ReportView(props: { readonly report: inspect.RenderedReport }) {
  const { report } = props;
  if (report.text !== null) {
    return <pre className="report-text overflow-x-auto overflow-y-hidden rounded-lg border bg-muted/40 p-3 font-mono text-xs dark:bg-black/25">{report.text}</pre>;
  }
  return (
    <div className="report overflow-x-auto overflow-y-hidden rounded-lg border">
      <table className="w-full text-left text-xs">
        <thead className="bg-muted/60 text-muted-foreground">
          <tr className="[&>th]:px-2.5 [&>th]:py-1.5 [&>th]:font-medium">
            {report.headers.map((header) => (
              <th key={header}>{header}</th>
            ))}
          </tr>
        </thead>
        <tbody className="font-mono [&>tr]:border-t [&_td]:px-2.5 [&_td]:py-1">
          {report.rows.map((row, index) => (
            <tr key={index}>
              {row.columns.map((value, column) => (
                <td key={column}>{value}</td>
              ))}
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

export function InspectPane() {
  const page = usePage();
  const t = useT();
  const [what, setWhat] = useState<inspect.InspectReport>("tasks");
  const [report, setReport] = useState<inspect.RenderedReport | null>(null);
  const [error, setError] = useState<string | null>(null);
  const run = async () => {
    const done = await attempt(() => page.ctx.client.call("inspect", inspect.toArgs([what])), false);
    if (done.ok) {
      setReport(inspect.renderReport(done.value.json, done.value.text));
      setError(null);
    } else {
      setError(done.error);
    }
  };
  const id = fieldId("inspect", "report");
  return (
    <div className="flex max-w-3xl flex-col gap-3">
      <Field id={id} label={t("inspect.report")}>
        <Select
          id={id}
          onChange={setWhat}
          options={inspect.INSPECT_REPORTS.map((section) => ({
            value: section.what,
            label: t(`inspect.section.${section.what}`),
            title: t(`inspect.section.${section.what}.summary`),
          }))}
          value={what}
        />
      </Field>
      <Actions>
        <Action data-action="inspect" onClick={run} variant="default">
          {t("inspect.run")}
        </Action>
      </Actions>
      {report === null ? null : <ReportView report={report} />}
      <Note>{t("inspect.nvsNote")}</Note>
      <ErrorLine error={error} />
    </div>
  );
}
