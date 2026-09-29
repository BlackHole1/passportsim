import { useNativeChange, usePage, useStore, useT } from "../hooks";
import { Action, Actions, ErrorLine, Field, Readout, fieldId } from "../controls";

export function SnapshotsCard() {
  const page = usePage();
  const t = useT();
  const view = useStore(page.snapshots.store);
  const points = page.rewind.list();
  const first = points[0];
  const last = points[points.length - 1];
  const stats = page.rewind.stats();
  const scrubber = useNativeChange<HTMLInputElement>((node) => {
    void page.snapshots.restore(Number(node.value));
  });
  const id = fieldId("snapshots", "rewind");
  const error =
    view.error === null ? null : view.error.kind === "refusal" ? view.error.text : t("snapshots.unsaved", { point: view.error.point });
  const label =
    view.label === null ? t("snapshots.none") : view.label.restorable ? view.label.text : t("snapshots.unsavedLabel", { point: view.label.text });
  const size = (stats.totalBytes / 1_000_000).toFixed(1);
  return (
    <>
      <p className="card-note text-muted-foreground text-xs">{t("snapshots.what")}</p>
      <Actions>
        <Action data-action="snapshot-save" onClick={() => page.snapshots.save()} variant="default">
          {t("snapshots.save")}
        </Action>
        <Action onClick={() => page.snapshots.fork()}>{t("snapshots.fork")}</Action>
        <Action onClick={() => page.snapshots.list()}>{t("snapshots.list")}</Action>
      </Actions>
      <Field id={id} label={t("snapshots.rewind")}>
        <input
          aria-label={t("snapshots.rewind.aria")}
          className="h-7 w-full min-w-0 cursor-pointer accent-foreground disabled:cursor-not-allowed disabled:opacity-50"
          disabled={points.length < 2}
          id={id}
          max={last?.seq ?? 1}
          min={first?.seq ?? 1}
          onInput={(event) => {
            page.snapshots.scrub(Number(event.currentTarget.value));
          }}
          ref={scrubber}
          step={1}
          type="range"
        />
      </Field>
      {view.saved === null ? null : (
        <Readout data-saved={view.saved.name}>{t("snapshots.lastSaved", { name: view.saved.name, at: view.saved.at })}</Readout>
      )}
      <Readout>{label}</Readout>
      <Readout>
        {stats.overTarget > 0
          ? t("snapshots.statsOver", { count: stats.count, size, over: stats.overTarget })
          : t("snapshots.stats", { count: stats.count, size })}
      </Readout>
      <ErrorLine error={error} />
    </>
  );
}
