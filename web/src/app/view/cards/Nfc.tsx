// The NFC card. Its copy of the tag changes only once the machine took the write. Locking is
// irreversible and confirmed first.

import { useRef, useState } from "react";
import type { NdefRecord } from "../../../api/commands";
import * as nfc from "../../panels/nfc";
import { attempt } from "../../controllers";
import type { Translate } from "../../i18n";
import { ndefRecord, type TapPreset } from "../../tap";
import { Action, Actions, ErrorLine, Field, Note, NumberInput, Readout, Select, TextInput, fieldId } from "../controls";
import { usePage, useT } from "../hooks";

function recordText(t: Translate, record: NdefRecord): string {
  switch (record.type) {
    case "uri":
      return `URI ${record.uri}`;
    case "text":
      return `Text ${record.lang ? `[${record.lang}] ` : ""}${record.text}`;
    case "wifi":
      return `Wi-Fi ${record.ssid} (${record.auth}/${record.encr}, ${t("nfc.keyLength", { count: record.key.length })})`;
  }
}

export function NfcCard() {
  const page = usePage();
  const t = useT();
  const client = page.ctx.client;
  const [tag, setTag] = useState(nfc.DEFAULT_NFC);
  const tagRef = useRef(nfc.DEFAULT_NFC);
  const [counter, setCounter] = useState<number | null>(null);
  const [error, setError] = useState<string | null>(null);
  const form = useRef({
    preset: "read" as TapPreset,
    record: "write-uri" as TapPreset,
    uri: "https://example.com/p",
    text: "",
    ssid: "",
    key: "",
    uid: "",
    frames: "",
  });
  const id = (name: string) => fieldId("nfc", name);
  const keep = (next: nfc.NfcState) => {
    tagRef.current = next;
    setTag(next);
  };

  const writeTag = async (next: nfc.NfcState) => {
    const done = await attempt(() => client.call("nfc_tag", nfc.toArgs(next)));
    if (done.ok) {
      keep(next);
    }
    setError(done.ok ? null : done.error);
  };
  const addRecord = async () => {
    // `tap.ts`'s builder, so a record written here and one written by a tap agree.
    let record: NdefRecord;
    try {
      const f = form.current;
      record = ndefRecord({ preset: f.record, uri: f.uri, text: f.text, ssid: f.ssid, key: f.key });
    } catch (thrown) {
      setError(thrown instanceof Error ? thrown.message : String(thrown));
      return;
    }
    await writeTag({ ...tagRef.current, records: [...tagRef.current.records, record] });
  };
  const setUid = async () => {
    const uid = form.current.uid.trim();
    if (uid.length > 0 && !nfc.isValidUid(uid)) {
      setError(t("nfc.badUid"));
      return;
    }
    await writeTag({ ...tagRef.current, uid: uid.length === 0 ? null : uid });
  };
  const armCounter = async () => {
    // The counter alone, so a rewrite of the records cannot ride along.
    const done = await attempt(() => client.call("nfc_tag", nfc.counterArgs()));
    if (done.ok) {
      keep({ ...tagRef.current, counter: true });
    }
    setError(done.ok ? null : done.error);
  };
  const tap = async () => {
    const f = form.current;
    const done = await attempt(async () => {
      const args = nfc.tapFor(tagRef.current, {
        preset: f.preset,
        uri: f.uri,
        text: f.text,
        ssid: f.ssid,
        key: f.key,
        frames: f.frames.split("\n"),
      });
      return client.call("nfc_tap", args);
    });
    if (done.ok) {
      const answered = (done.value.json as { counter?: unknown } | null)?.counter;
      if (typeof answered === "number") {
        setCounter(answered);
      }
      page.actions.tapped();
    }
    setError(done.ok ? null : done.error);
  };
  const lock = async () => {
    if (!page.ctx.confirm(t("nfc.lockConfirm"))) {
      return;
    }
    // `lockArgs()` alone: a lock carrying `ndef` would rewrite the tag in the same call.
    const done = await attempt(() => client.call("nfc_tag", nfc.lockArgs()));
    if (done.ok) {
      keep({ ...tagRef.current, locked: true });
    }
    setError(done.ok ? null : done.error);
  };

  const text = (name: keyof typeof form.current, label: string, hint?: string, placeholder?: string) => (
    <Field hint={hint} id={id(name)} label={label}>
      <TextInput
        id={id(name)}
        onCommit={(value) => {
          form.current = { ...form.current, [name]: value };
        }}
        placeholder={placeholder}
        value={form.current[name]}
      />
    </Field>
  );

  return (
    <>
      <ul aria-label={t("nfc.records.aria")} className="ndef-list flex flex-col gap-1 text-xs empty:hidden">
          {tag.records.map((record, index) => (
            <li className="flex items-center gap-2 rounded-md bg-muted px-2 py-1" key={index}>
              <span className="min-w-0 flex-1 truncate font-mono">{recordText(t, record)}</span>
              <Action
                disabled={tag.locked}
                onClick={() => writeTag({ ...tagRef.current, records: tagRef.current.records.filter((_, at) => at !== index) })}
                variant="ghost"
              >
                {t("common.remove")}
              </Action>
            </li>
          ))}
      </ul>
      <Field id={id("preset")} label={t("nfc.preset")}>
        <Select
          id={id("preset")}
          onChange={(preset) => {
            form.current = { ...form.current, preset };
          }}
          options={[
            { value: "read", label: t("nfc.preset.read") },
            { value: "write-uri", label: t("nfc.preset.uri") },
            { value: "write-text", label: t("nfc.preset.text") },
            { value: "write-wifi", label: t("nfc.preset.wifi") },
            { value: "raw", label: t("nfc.preset.raw") },
            { value: "tear", label: t("nfc.preset.tear") },
          ]}
          value={form.current.preset}
        />
      </Field>
      <Field id={id("record")} label={t("nfc.record")}>
        <Select
          id={id("record")}
          onChange={(record) => {
            form.current = { ...form.current, record };
          }}
          options={[
            { value: "write-uri", label: "URI" },
            { value: "write-text", label: "Text" },
            { value: "write-wifi", label: "Wi-Fi" },
          ]}
          value={form.current.record}
        />
      </Field>
      {text("uri", "URI")}
      {text("text", t("nfc.text"))}
      {text("ssid", "SSID")}
      {text("key", t("nfc.key"))}
      {text("uid", "UID", t("nfc.uidHint"))}
      {text("frames", t("nfc.frames"), undefined, "60 3004")}
      <Field hint="ms" id={id("dwell")} label={t("nfc.dwell")}>
        <NumberInput
          id={id("dwell")}
          max={5_000}
          min={1}
          onCommit={(dwellMs) => {
            keep({ ...tagRef.current, dwellMs });
          }}
          value={tag.dwellMs}
        />
      </Field>
      <Actions>
        <Action data-action="tap" onClick={tap} variant="default">
          {t("nfc.tap")}
        </Action>
        <Action data-action="ndef-add" disabled={tag.locked} onClick={addRecord}>
          {t("nfc.addRecord")}
        </Action>
        <Action disabled={tag.locked} onClick={setUid}>
          {t("nfc.setUid")}
        </Action>
        <Action data-action="nfc-counter" onClick={armCounter}>
          {t("nfc.armCounter")}
        </Action>
        <Action onClick={lock}>{t("nfc.lock")}</Action>
      </Actions>
      <div className="flex flex-col gap-1">
        <Readout>{tag.locked
            ? t("nfc.locked")
            : tag.records.length === 1
              ? t("nfc.recordOne")
              : t("nfc.recordMany", { count: tag.records.length })}</Readout>
        <Readout data-counter={counter === null ? "" : String(counter)} data-nfc="counter">
          {counter !== null ? t("nfc.counter", { count: counter }) : tag.counter ? t("nfc.counterArmed") : t("nfc.counterOff")}
        </Readout>
      </div>
      <Note>{t("nfc.note")}</Note>
      <ErrorLine error={error} />
    </>
  );
}
