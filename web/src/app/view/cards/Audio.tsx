// The Audio card. A `null` output report is said as "no audio path", since a meter pinned at the
// floor would look the same.

import { useRef, useState } from "react";
import type { AudioCaptureResult } from "../../../api/commands";
import * as audio from "../../panels/audio";
import { attempt } from "../../controllers";
import type { Translate } from "../../i18n";
import { Action, Actions, ErrorLine, Field, Note, NumberInput, Readout, Select, TextInput, fieldId } from "../controls";
import { usePage, useStore, useT } from "../hooks";

export function outputText(t: Translate, report: audio.AudioOutput | null): string {
  if (report === null) {
    return t("audio.noPath");
  }
  const db = report.peak > 0 ? audio.dbfs(report.peak).toFixed(1) : "-inf";
  const playback =
    report.quanta === null
      ? t("audio.noWorklet")
      : t("audio.playback", { quanta: report.quanta, underruns: report.underruns ?? 0, silent: report.starvedQuanta ?? 0 });
  return t("audio.output", { db, pushed: report.pushed, playback });
}

function captureText(t: Translate, json: unknown): string {
  const analysis = (json as AudioCaptureResult | null)?.analysis;
  if (typeof json !== "object" || json === null || !analysis) {
    return t("audio.noSamples");
  }
  const result = json as AudioCaptureResult;
  const hz = analysis.fundamental_hz === null ? t("audio.noFundamental") : `${analysis.fundamental_hz} Hz`;
  const text = t("audio.captured", { hz, peak: analysis.peak });
  return result.dropped_samples ? `${text}, ${t("audio.dropped", { count: result.dropped_samples })}` : text;
}

export function AudioCard() {
  const page = usePage();
  const t = useT();
  const output = useStore(page.audio);
  const state = useRef(audio.DEFAULT_AUDIO);
  const [source, setSource] = useState(audio.DEFAULT_AUDIO.source);
  const [capture, setCapture] = useState("");
  const [error, setError] = useState<string | null>(null);
  const id = (name: string) => fieldId("audio", name);

  const update = (patch: Partial<audio.AudioState>) => {
    state.current = { ...state.current, ...patch };
    setSource(state.current.source);
    void attempt(() => page.ctx.client.call("mic_set", audio.toArgs(state.current))).then((done) => {
      setError(done.ok ? null : done.error);
    });
  };
  const runCapture = async () => {
    const args = audio.captureArgs(state.current);
    const done = await attempt(() => page.ctx.client.call("audio_capture", args));
    if (done.ok) {
      state.current = audio.fromCaptureArgs(args, state.current);
      setCapture(captureText(t, done.value.json));
    }
    setError(done.ok ? null : done.error);
  };
  const value = output === null ? 0 : audio.meterValue(output.peak);

  return (
    <>
      <Field id={id("mic-source")} label={t("audio.source")}>
        <Select
          id={id("mic-source")}
          onChange={(next) => update({ source: next })}
          options={audio.MIC_SOURCES.map((option) => ({
            value: option.id,
            label: option.id,
            title: t(`audio.source.${option.id}`),
          }))}
          value={source}
        />
      </Field>
      <div className="flex flex-col gap-3" hidden={source !== "tone"}>
        <Field hint="Hz" id={id("tone")} label={t("audio.tone")}>
          <NumberInput id={id("tone")} max={audio.TONE_RANGE.max} min={audio.TONE_RANGE.min} onCommit={(toneHz) => update({ toneHz })} value={state.current.toneHz} />
        </Field>
        <Field id={id("amplitude")} label={t("audio.amplitude")}>
          <NumberInput
            id={id("amplitude")}
            max={audio.AMPLITUDE_RANGE.max}
            min={audio.AMPLITUDE_RANGE.min}
            onCommit={(amplitude) => update({ amplitude })}
            value={state.current.amplitude}
          />
        </Field>
      </div>
      <div hidden={source !== "file"}>
        <Field id={id("file")} label={t("audio.file")}>
          <TextInput id={id("file")} onCommit={(fileName) => update({ fileName })} value={state.current.fileName} />
        </Field>
      </div>
      <Field hint="ms" id={id("capture")} label={t("audio.capture")}>
        <NumberInput
          id={id("capture")}
          max={audio.CAPTURE_MS_RANGE.max}
          min={audio.CAPTURE_MS_RANGE.min}
          onCommit={(captureMs) => {
            state.current = { ...state.current, captureMs };
          }}
          value={state.current.captureMs}
        />
      </Field>
      <Actions>
        <Action data-action="audio-capture" onClick={runCapture}>
          {t("audio.captureRun")}
        </Action>
      </Actions>
      {capture === "" ? null : <Readout>{capture}</Readout>}
      <div className="field flex flex-col gap-1.5">
        <meter
          aria-label={t("audio.meter.aria")}
          className="audio-meter h-2 w-full"
          max={1}
          min={0}
          value={Number(value.toFixed(4))}
        />
        <Readout>{outputText(t, output)}</Readout>
      </div>
      <Note>{t("audio.note")}</Note>
      <ErrorLine error={error} />
    </>
  );
}
