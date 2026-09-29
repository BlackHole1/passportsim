import type { Translate } from "../../i18n";
import type { UiTreeState } from "../../panels/uiTree";
import { cn } from "../../../ui/lib/utils";
import { ErrorLine } from "../controls";
import { usePage, useStore, useT } from "../hooks";

export function uiSummary(t: Translate, state: UiTreeState): string {
  if (state.rev === null) {
    return t("ui.notRead");
  }
  const parts = [`ui_rev ${state.rev}`, t("ui.lines", { count: state.rows.length })];
  if (state.objects !== null) {
    parts.push(t("ui.objects", { count: state.objects }));
  }
  if (state.settled === false) {
    parts.push(t("ui.unsettled"));
  }
  if (state.truncated) {
    parts.push(t("ui.truncated"));
  }
  return parts.join(" | ");
}

export function UiTreePane() {
  const page = usePage();
  const t = useT();
  const view = useStore(page.uiTree.store);
  const hover = (ref: string | null) => {
    page.uiTree.hover(ref);
  };
  return (
    <div className="flex flex-col gap-3" data-ui-tree="">
      <p className="card-note text-muted-foreground text-xs" data-ui-summary="">
        {uiSummary(t, view.state)}
      </p>
      <div
        aria-label={t("ui.tree.aria")}
        className="ui-tree max-h-[32rem] overflow-auto rounded-lg border bg-muted/40 px-3 py-2 font-mono text-xs leading-5 dark:bg-black/25"
        data-scroller=""
        role="tree"
      >
        {view.state.rows.map((row, index) => {
          const boxed = row.rect !== null;
          return (
            <div
              aria-level={row.depth + 1}
              className={cn(
                "ui-row whitespace-pre rounded-sm",
                row.cls === null && "ui-note text-muted-foreground italic",
                boxed && "cursor-default hover:bg-foreground/8 focus:bg-foreground/8 focus:outline-none",
              )}
              data-class={row.cls ?? undefined}
              data-line={index}
              data-ref={row.ref ?? undefined}
              key={`${index}-${row.ref ?? ""}`}
              onBlur={boxed ? () => hover(null) : undefined}
              onFocus={boxed ? () => hover(row.ref) : undefined}
              onMouseEnter={boxed ? () => hover(row.ref) : undefined}
              onMouseLeave={boxed ? () => hover(null) : undefined}
              role="treeitem"
              style={{ paddingLeft: `${row.depth * 1.25}em` }}
              tabIndex={boxed ? -1 : undefined}
            >
              {row.raw}
            </div>
          );
        })}
      </div>
      <ErrorLine error={view.error} />
    </div>
  );
}
