// `env` replaces the whole scripted air, so every change sends the full AP list. A key is sent and
// never rendered.

import { useRef, useState } from "react";
import * as wifi from "../../panels/wifi";
import { attempt } from "../../controllers";
import { Action, Actions, Checkbox, ErrorLine, Field, Note, NumberInput, Readout, TextInput, fieldId } from "../controls";
import { usePage, useT } from "../hooks";
import { RadioUnbound, useRadioBinding } from "../notices";

export function WifiCard() {
  const page = usePage();
  const t = useT();
  const client = page.ctx.client;
  const [state, setState] = useState(wifi.DEFAULT_WIFI);
  const current = useRef(wifi.DEFAULT_WIFI);
  const draft = useRef({ ssid: "", channel: 1, rssi: -55, key: "" });
  const [error, setError] = useState<string | null>(null);
  const binding = useRadioBinding("wifi");
  const id = (name: string) => fieldId("wifi", name);
  const keep = (next: wifi.WifiState) => {
    current.current = next;
    setState(next);
  };

  const setAps = async (aps: readonly wifi.AccessPoint[]) => {
    const done = await attempt(() => client.call("env", wifi.envArgs(aps)));
    if (done.ok) {
      keep({ ...current.current, aps: [...aps] });
    }
    setError(binding.settle(done));
  };
  const addAp = async () => {
    let ap: wifi.AccessPoint;
    try {
      ap = wifi.normalizeAp(draft.current, current.current.aps.length);
    } catch (thrown) {
      setError(thrown instanceof Error ? thrown.message : String(thrown));
      return;
    }
    await setAps([...current.current.aps, ap]);
  };
  const savePcap = async () => {
    const done = await attempt(() => client.call("net_capture", wifi.captureArgs()));
    if (done.ok) {
      const out = done.value as { capture?: { path?: string } | null };
      keep({ ...current.current, capture: out?.capture?.path ?? null });
    }
    setError(binding.settle(done));
  };

  return (
    <>
      {binding.unbound ? <RadioUnbound radio="wifi" /> : null}
      <ul aria-label={t("wifi.aps.aria")} className="ap-list flex flex-col gap-1 text-xs empty:hidden">
        {state.aps.map((ap) => (
          <li className="flex items-center gap-2 rounded-md bg-muted px-2 py-1" key={ap.bssid}>
            <span className="min-w-0 flex-1 truncate font-mono">{`${ap.ssid} ${ap.bssid} ch${ap.channel} ${ap.rssi} dBm ${ap.auth}`}</span>
            <Action onClick={() => setAps(current.current.aps.filter((other) => other !== ap))} variant="ghost">
              {t("common.remove")}
            </Action>
          </li>
        ))}
      </ul>
      <Field id={id("ssid")} label="SSID">
        <TextInput
          id={id("ssid")}
          onCommit={(ssid) => {
            draft.current = { ...draft.current, ssid };
          }}
          value=""
        />
      </Field>
      <Field id={id("channel")} label={t("wifi.channel")}>
        <NumberInput
          id={id("channel")}
          max={wifi.CHANNEL_RANGE.max}
          min={wifi.CHANNEL_RANGE.min}
          onCommit={(channel) => {
            draft.current = { ...draft.current, channel };
          }}
          value={1}
        />
      </Field>
      <Field hint="dBm" id={id("rssi")} label="RSSI">
        <NumberInput
          id={id("rssi")}
          max={-1}
          min={-110}
          onCommit={(rssi) => {
            draft.current = { ...draft.current, rssi };
          }}
          value={-55}
        />
      </Field>
      <Field id={id("key")} label={t("wifi.key")}>
        <TextInput
          autoComplete="new-password"
          id={id("key")}
          onCommit={(key) => {
            draft.current = { ...draft.current, key };
          }}
          placeholder={t("wifi.keyHint")}
          value=""
        />
      </Field>
      <Actions>
        <Action onClick={addAp} variant="default">
          {t("wifi.addAp")}
        </Action>
        <Action onClick={savePcap}>{t("wifi.savePcap")}</Action>
      </Actions>
      {state.capture === null ? null : <Readout>{state.capture}</Readout>}
      {/* The bridge needs a route the card has no form for yet, so the toggle stays off rather
          than inventing a destination; the note names the command that opens one. */}
      <Field id={id("bridge")} label={t("wifi.bridge")}>
        <Checkbox checked={state.bridge} disabled id={id("bridge")} onChange={() => undefined} />
      </Field>
      <Note>{t("wifi.bridgeWarning")}</Note>
      <Note>{t("wifi.bridgeUnwired")}</Note>
      <Note>{t("wifi.note")}</Note>
      <ErrorLine error={error} />
    </>
  );
}
