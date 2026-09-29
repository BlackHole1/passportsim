import { useEffect, useRef, useState } from "react";
import * as battery from "../../panels/battery";
import { attempt } from "../../controllers";
import { Action, Actions, Checkbox, ErrorLine, Field, Note, NumberInput, RangeInput, Readout, fieldId } from "../controls";
import { usePage, useStore, useT } from "../hooks";

export function BatteryCard() {
  const page = usePage();
  const t = useT();
  const client = page.ctx.client;
  const usb = useStore(page.usb.store);
  const generation = useStore(page.generation);
  const state = useRef(battery.DEFAULT_BATTERY);
  const [shown, setShown] = useState(battery.DEFAULT_BATTERY);
  // Remount an input only when another control moved its value; remounting the focused one steals focus.
  const [revision, setRevision] = useState({ soc: 0, mv: 0 });
  const [error, setError] = useState<string | null>(null);
  const id = (name: string) => fieldId("battery", name);

  const show = (next: battery.BatteryState, from: "soc" | "mv" | "other") => {
    const before = state.current;
    state.current = next;
    setShown(next);
    setRevision((current) => ({
      soc: current.soc + (from !== "soc" && next.soc !== before.soc ? 1 : 0),
      mv: current.mv + (from !== "mv" && next.mv !== before.mv ? 1 : 0),
    }));
  };

  // A new machine starts with a new board, whose cell is full.
  useEffect(() => {
    if (generation > 0) {
      show(battery.DEFAULT_BATTERY, "other");
      setError(null);
    }
  }, [generation]);

  const change = async (next: battery.BatteryChange, from: "soc" | "mv" | "other") => {
    const done = await attempt(() => client.call("env", battery.changeArgs(next)));
    if (done.ok) {
      show(battery.applyChange(state.current, next), from);
    } else {
      setRevision((current) => ({ soc: current.soc + 1, mv: current.mv + 1 }));
    }
    setError(done.ok ? null : (done.error ?? null));
  };
  const fresh = async () => {
    // A fresh gauge is a disconnect and a reconnect, in order.
    const done = await attempt(async () => {
      for (const args of battery.FRESH_ARGS) {
        await client.call("env", args);
      }
    });
    if (done.ok) {
      show(battery.FRESH_ARGS.reduce((cell, args) => battery.fromArgs(args, cell), state.current), "other");
    }
    setError(done.ok ? null : (done.error ?? null));
  };

  // The charger is the USB cable, moved through the USB card's controller so both cards and the
  // strip agree.
  const cable = usb.id !== "U0";
  return (
    <>
      <Field hint="%" id={id("soc")} label={t("battery.soc")}>
        <RangeInput
          id={id("soc")}
          key={`soc-range-${revision.soc}`}
          max={battery.SOC_RANGE.max}
          min={battery.SOC_RANGE.min}
          onCommit={(soc) => void change({ kind: "soc", soc }, "soc")}
          value={shown.soc}
        />
        <span className="w-20 shrink-0">
          <NumberInput
            id={id("soc-number")}
            key={`soc-number-${revision.soc}-${shown.soc}`}
            max={battery.SOC_RANGE.max}
            min={battery.SOC_RANGE.min}
            onCommit={(soc) => void change({ kind: "soc", soc }, "other")}
            value={shown.soc}
          />
        </span>
      </Field>
      <Field hint="mV" id={id("vcell")} label={t("battery.vcell")}>
        <NumberInput
          id={id("vcell")}
          key={`vcell-${revision.mv}`}
          max={battery.MV_RANGE.max}
          min={battery.MV_RANGE.min}
          onCommit={(mv) => void change({ kind: "mv", mv }, "mv")}
          value={shown.mv}
        />
      </Field>
      <Field hint="°C" id={id("temp")} label={t("battery.temp")}>
        <NumberInput
          id={id("temp")}
          max={battery.TEMP_RANGE.max}
          min={battery.TEMP_RANGE.min}
          onCommit={(tempC) => void change({ kind: "temp", tempC }, "other")}
          value={shown.tempC}
        />
      </Field>
      <Field hint={t("battery.charger.hint")} id={id("charger")} label={t("battery.charger")}>
        <Checkbox
          checked={cable}
          id={id("charger")}
          key={`charger-${cable}`}
          onChange={(plugged) => {
            void page.usb.select(plugged ? "U3" : "U0");
          }}
        />
      </Field>
      <Field id={id("connected")} label={t("battery.connected")}>
        <Checkbox
          checked={shown.connected}
          id={id("connected")}
          key={`connected-${shown.connected}`}
          onChange={(connected) => void change({ kind: "connected", connected }, "other")}
        />
      </Field>
      <Actions>
        <Action onClick={fresh}>{t("battery.fresh")}</Action>
      </Actions>
      <Readout data-battery-cell="">{t("battery.cell", { soc: shown.soc, mv: shown.mv, temp: shown.tempC })}</Readout>
      {shown.connected ? null : <Note>{t("battery.disconnected")}</Note>}
      <Note>{t("battery.reads")}</Note>
      <Note>{t("battery.note")}</Note>
      <ErrorLine error={error} />
    </>
  );
}
