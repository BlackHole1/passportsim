import { cn } from "../../../ui/lib/utils";
import { RAIL_ONLY_STATES, USB_STATES } from "../../skin";
import { ErrorLine, Note, Readout } from "../controls";
import { usePage, useStore, useT } from "../hooks";

export function UsbCard() {
  const page = usePage();
  const t = useT();
  const usb = useStore(page.usb.store);
  const railOnly = new Set<string>(RAIL_ONLY_STATES);
  const current = USB_STATES.find((info) => info.id === usb.id);
  return (
    <>
      <Note>{t("usb.legend")}</Note>
      <div className="card-actions grid grid-cols-2 gap-1 rounded-lg bg-muted p-0.5">
        {USB_STATES.map((info) => {
          const blocked = railOnly.has(info.id);
          const label = t(`usb.label.${info.id}`);
          return (
            <button
              aria-disabled={blocked ? "true" : undefined}
              aria-label={t("usb.state.aria", { id: info.id, label })}
              aria-pressed={usb.id === info.id ? "true" : "false"}
              className={cn(
                "flex min-h-11 min-w-0 flex-col items-start justify-center rounded-md px-3 py-1.5 text-start text-muted-foreground text-sm transition-colors hover:text-foreground",
                usb.id === info.id && "bg-background text-foreground shadow-xs/5 dark:bg-input",
                blocked && "cursor-not-allowed opacity-56 hover:text-muted-foreground",
              )}
              data-usb-card={info.id}
              key={info.id}
              // U1 is the cable in with the rail off, and the rail is the power button's, so it is explained,
              // never sent. `aria-disabled` keeps the explanation reachable in every engine.
              onClick={() => {
                if (!blocked) {
                  void page.usb.select(info.id);
                }
              }}
              title={blocked ? t("usb.u1Why") : t(`usb.summary.${info.id}`)}
              type="button"
            >
              <span className="w-full truncate font-medium">{label}</span>
              <span className="font-mono text-muted-foreground text-xs">{info.id}</span>
            </button>
          );
        })}
      </div>
      <Readout data-usb-now={usb.id}>{`${usb.id} ${current?.name ?? ""}: ${t(`usb.summary.${usb.id}`)}`}</Readout>
      <Note>{t("usb.u1Why")}</Note>
      <ErrorLine error={usb.error} />
    </>
  );
}
