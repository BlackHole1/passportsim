import { describe, expect, test } from "bun:test";
import { existsSync, readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { edgeArgs } from "../app/controls";
import { EventKind } from "../worker/layout";
import { CommandClient } from "./client";
import { copyAsCli, copyAsStep } from "./copy";
import type { Json } from "./envelope";
import { UiJournal } from "./journal";
import { durationOf, exportScenario, ScenarioRecorder } from "./recorder";
import { SECRET, SecretValues, redactCall, sessionSecrets } from "./redact";
import { parseScenarioYaml } from "./scenarioReader";

const ROOT = join(dirname(fileURLToPath(import.meta.url)), "..", "..", "..");
const MS = 1_000_000_000n;

function harness(refuse: readonly string[] = []) {
  const journal = new UiJournal();
  let now = 0n;
  const client = new CommandClient(
    async (request) => {
      const cmd = (JSON.parse(request) as { cmd: string }).cmd;
      return refuse.includes(cmd)
        ? { err: JSON.stringify({ code: "E_LEASE", message: "held by agent" }) }
        : { ok: JSON.stringify({ text: "ok", json: {} }) };
    },
    { journal, nowPs: () => now },
  );
  return { journal, client, at: (ps: bigint) => (now = ps) };
}

describe("redaction", () => {
  test("NFC UID, a Wi-Fi NDEF key and a PWD_AUTH frame are replaced, by field and then by value", () => {
    const tag = { uid: "04A1B2C3D4E5F6", ndef: [{ type: "wifi", ssid: "lab", auth: "wpa2", encr: "aes", key: "hunter2hunter2" }] };
    const tap = { ops: [{ op: "raw", frames: ["1b a1b2c3d4", "30 04"] }] };
    const write = { op: "write", text: "wifi set lab hunter2hunter2 a1b2c3d4" };
    const calls = [
      { command: "nfc_tag" as const, args: tag },
      { command: "nfc_tap" as const, args: tap },
      { command: "serial" as const, args: write },
    ];
    const secrets = sessionSecrets(calls);
    expect(redactCall("nfc_tag", tag, secrets)).toEqual({
      args: { uid: SECRET, ndef: [{ type: "wifi", ssid: "lab", auth: "wpa2", encr: "aes", key: SECRET }] },
      redacted: ["uid", "ndef/0/key"],
      refused: null,
    });
    expect(redactCall("nfc_tap", tap, secrets).args).toEqual({ ops: [{ op: "raw", frames: [SECRET, "30 04"] }] });
    expect(redactCall("serial", write, secrets)).toEqual({
      args: { op: "write", text: `wifi set lab ${SECRET} ${SECRET}` },
      redacted: ["text"],
      refused: null,
    });
    // The input is untouched: the journal keeps what the user did.
    expect(tag.uid).toBe("04A1B2C3D4E5F6");
  });

  test("user-supplied addresses are never masked by shape", () => {
    const ap = { ssid: "lab", bssid: "02:00:00:aa:bb:cc" };
    expect(redactCall("wifi_ap", ap).args).toEqual(ap);
  });

  test("a confirmation code or an unredacted export is refused, never rendered", () => {
    const confirm = { op: "export", name: "menu", include_secrets: true, confirm: "K7Q2" };
    const journal = [{ seq: 1, command: "snapshot" as const, args: confirm, vtPs: 0n, outcome: { state: "ok" as const, text: "", elapsedVtUs: null } }];
    const cli = copyAsCli(journal[0]!, sessionSecrets(journal), "sh");
    const step = copyAsStep(journal[0]!, sessionSecrets(journal));
    expect(cli.ok).toBe(false);
    expect(step.ok).toBe(false);
    expect(JSON.stringify([cli, step])).not.toContain("K7Q2");
  });
});

describe("the scenario recorder", () => {
  test("a session exports as exactly this scenario@1 document", async () => {
    const { journal, client, at } = harness(["env"]);
    const recorder = new ScenarioRecorder(journal);
    await client.tryCall("input", edgeArgs("up", true)); // before recording: not in the file
    recorder.start({ image: "official", power: "on", usb: "open" });
    // A click on the skin is a press and, one minimum hold of guest time later, a release.
    at(1_000n * MS);
    await client.tryCall("input", edgeArgs("down", true));
    at(1_080n * MS);
    await client.tryCall("input", edgeArgs("down", false));
    recorder.observe([
      { kind: EventKind.Frame, vtPs: 1_090n * MS },
      { kind: EventKind.UiSettled, vtPs: 1_120n * MS },
    ]);
    at(1_500n * MS);
    await client.tryCall("ui", {}); // read-only
    await client.tryCall("env", { usb: "unplugged" }); // refused by the transport
    await client.tryCall("clock", { op: "pause" }); // no step
    at(2_000n * MS);
    await client.tryCall("input", edgeArgs("ok", true));
    at(2_743n * MS);
    await client.tryCall("input", edgeArgs("ok", false));
    const out = recorder.stop("down-then-ok", 2_993n * MS);
    expect(recorder.recording).toBe(false);
    expect(out?.yaml).toBe(
      [
        "# Recorded in the PassportSim web UI. Review before committing it under tests/scenarios/.",
        "# skipped 1x: `ui`: read-only",
        "# skipped 1x: `env`: refused by the registry",
        "# skipped 1x: `clock` claims no scenario step (its CommandSpec::scenario_step is None)",
        "schema: passportsim/scenario@1",
        "name: down-then-ok",
        "image: official",
        "setup: {power: on, usb: open}",
        "steps:",
        "  - name: 'input (journal #2)'",
        "    press: {button: down, action: press}",
        "  - delay: 80ms",
        "  - name: 'input (journal #3)'",
        "    press: {button: down, action: release}",
        "  - name: 'the ui_changed that followed'",
        "    wait: 'event:ui_changed'",
        "  - delay: 880ms",
        "  - name: 'input (journal #7)'",
        "    press: {button: ok, action: press}",
        "  - delay: 743ms",
        "  - name: 'input (journal #8)'",
        "    press: {button: ok, action: release}",
        "  - delay: 250ms",
        "",
      ].join("\n"),
    );
    expect(out?.steps).toBe(9);
  });

  test("the export reads back through the scenario@1 grammar as the recorded calls", async () => {
    const { journal, client, at } = harness();
    const recorder = new ScenarioRecorder(journal);
    recorder.start({ image: null, power: "on", usb: null });
    at(5n * MS);
    await client.tryCall("env", { battery: { mv: 3900, soc: 80 } });
    await client.tryCall("serial", { op: "write", text: "hello: world # not a comment", newline: true });
    await client.tryCall("snapshot", { op: "save", name: "7" });
    const out = recorder.stop("round trip", null);
    const doc = parseScenarioYaml(out?.yaml ?? "") as Record<string, Json>;
    expect(doc.schema).toBe("passportsim/scenario@1");
    expect(doc.name).toBe("round trip");
    expect(doc.setup).toEqual({ power: "on" });
    expect((doc.steps as Array<Record<string, Json>>).map(({ name: _name, ...rest }) => rest)).toEqual([
      { env: { battery: { mv: 3900, soc: 80 } } },
      { "serial.write": { op: "write", text: "hello: world # not a comment", newline: true } },
      { snapshot: { op: "save", name: "7" } },
    ]);
  });

  test("the port of the reader agrees with the committed scenarios on their steps", () => {
    const doc = parseScenarioYaml(readFileSync(join(ROOT, "tests", "scenarios", "env-journal.yaml"), "utf8")) as {
      setup: Json;
      steps: Array<Record<string, Json>>;
    };
    expect(doc.setup).toEqual({ power: "on" });
    expect(doc.steps[0]).toEqual({ name: "the cell reads 3900 mV at 80 percent", env: { battery: { mv: 3900, soc: 80 } } });
    expect(doc.steps.at(-1)).toMatchObject({ delay: "10ms" });
  });

  test("a Wi-Fi NDEF key typed again in the console, and a confirmation code, are absent from the YAML", async () => {
    const { journal, client, at } = harness();
    const recorder = new ScenarioRecorder(journal);
    recorder.start({ image: "official", power: "on", usb: "open" });
    at(MS);
    await client.tryCall("nfc_tag", { ndef: [{ type: "wifi", ssid: "lab", auth: "wpa2", encr: "aes", key: "correct-horse" }] });
    await client.tryCall("serial", { op: "write", text: "join lab correct-horse" });
    await client.tryCall("snapshot", { op: "export", name: "menu", include_secrets: true, confirm: "K7Q2" });
    const yaml = recorder.stop("secret", 2n * MS)?.yaml ?? "";
    expect(yaml).not.toContain("correct-horse");
    expect(yaml).not.toContain("K7Q2");
    expect(yaml).toContain("- serial.write: {op: write, text: 'join lab <SECRET>'}".replace("- ", "  "));
    // `nfc_tag` is registered, so the load is a step too and its key is redacted in place.
    expect(yaml).toContain("# secrets replaced by <SECRET>: nfc_tag ndef/0/key, serial text");
    expect(yaml).toContain("nfc.tag: {ndef: [{type: wifi, ssid: lab, auth: wpa2, encr: aes, key: '<SECRET>'}]}");
  });

  test("entries lost to the journal limit are declared, not silently missing", () => {
    const journal = new UiJournal(2);
    const recorder = new ScenarioRecorder(journal);
    recorder.start({ image: null, power: "on", usb: null });
    for (let i = 0; i < 5; i++) {
      journal.record("input", { button: "ok", action: "click" }, 0n).settle({ ok: true, text: "" });
    }
    expect(recorder.stop("lossy", null)?.notes[0]).toContain("3 earlier calls");
  });

  test("durations use the Duration grammar at 1 us resolution", () => {
    expect(durationOf(250n * MS)).toBe("250ms");
    expect(durationOf(1_250_000_000n)).toBe("1250us");
    expect(durationOf(999_999n)).toBeNull();
  });

  // The real scenario reader, through the CLI, when one is built.
  const cli = process.env.PEMU_CLI ?? join(ROOT, "target", "debug", process.platform === "win32" ? "passportsim.exe" : "passportsim");
  const validate = existsSync(cli) ? test : test.skip;
  validate(
    `\`passportsim scenario --op validate\` accepts the export (skipped when ${cli} is not built: run \`cargo build -p pemu-cli\`)`,
    () => {
      const out = exportScenario({
        name: "validated",
        description: "every step form the recorder writes",
        start: { image: null, power: "on", usb: "open" },
        calls: [
          { command: "input", args: { button: "ok", action: "click" } },
          { command: "env", args: { mic: { kind: "tone", hz: 440 } } },
          { command: "run", args: { until: "serial:/pk_app: ready/", timeout: "5s" } },
          { command: "serial", args: { op: "write", text: "it's \"quoted\"\tand tabbed" } },
          { command: "snapshot", args: { op: "save", name: "menu" } },
          { command: "ui", args: { diff: 1 } },
        ].map((call, index) => ({
          seq: index + 1,
          command: call.command as "input",
          args: call.args as unknown as Json,
          vtPs: BigInt(index) * MS,
          outcome: { state: "ok" as const, text: "", elapsedVtUs: null },
        })),
        events: [],
        endPs: 10n * MS,
        lost: 0,
        secrets: new SecretValues(),
      });
      const run = Bun.spawnSync([cli, "scenario", "--output", "json", "--json", "-"], {
        stdin: new TextEncoder().encode(JSON.stringify({ op: "validate", inline: out.yaml })),
      });
      const text = run.stdout.toString();
      expect({ exit: run.exitCode, text }).toMatchObject({ exit: 0 });
      expect(text).toContain('"status":"pass"');
    },
  );
});

describe("one secret set for the whole session", () => {
  const wifiTag = { ndef: [{ type: "wifi" as const, ssid: "lab", auth: "wpa2", encr: "aes", key: "correct-horse" }] };

  test("a key entered before Record and typed during it is masked in the export", async () => {
    const { journal, client, at } = harness();
    const recorder = new ScenarioRecorder(journal);
    await client.tryCall("nfc_tag", wifiTag);
    recorder.start({ image: "official", power: "on", usb: "open" });
    at(MS);
    await client.tryCall("serial", { op: "write", text: "join lab correct-horse" });
    const yaml = recorder.stop("before", 2n * MS)?.yaml ?? "";
    expect(yaml).toContain("serial.write");
    expect(yaml).not.toContain("correct-horse");
  });

  test("a key whose entry fell off the journal ring is still masked by Copy as CLI", async () => {
    const journal = new UiJournal(2);
    const client = new CommandClient(async () => ({ ok: JSON.stringify({ text: "", json: {} }) }), { journal });
    await client.tryCall("nfc_tag", wifiTag);
    for (let i = 0; i < 3; i++) {
      await client.tryCall("input", { button: "ok", action: "click" });
    }
    await client.tryCall("serial", { op: "write", text: "join lab correct-horse" });
    expect(journal.list().some((record) => record.command === "nfc_tag")).toBe(false);
    const record = journal.last();
    if (!record) throw new Error("nothing journaled");
    const copied = copyAsCli(record, journal.secrets, "sh");
    expect(JSON.stringify(copied)).not.toContain("correct-horse");
    expect(JSON.stringify(copyAsStep(record, journal.secrets))).not.toContain("correct-horse");
  });
});

describe("a call's own virtual time", () => {
  test("the delay after a hold excludes the time the hold itself advanced", async () => {
    const journal = new UiJournal();
    let now = 0n;
    const client = new CommandClient(
      async () => ({ ok: JSON.stringify({ text: "", json: { elapsed_vt_us: 740_000 } }) }),
      { journal, nowPs: () => now },
    );
    const recorder = new ScenarioRecorder(journal);
    recorder.start({ image: null, power: "on", usb: null });
    now = 1_000n * MS;
    await client.tryCall("input", { button: "ok", action: "hold", duration: 740 });
    expect(journal.last()?.outcome).toEqual({ state: "ok", text: "", elapsedVtUs: 740_000 });
    now = 2_000n * MS;
    await client.tryCall("input", { button: "ok", action: "click" });
    const yaml = recorder.stop("hold", 2_000n * MS)?.yaml ?? "";
    // 1000 ms between the stamps, 740 ms of which the hold replays by itself.
    expect(yaml).toContain("  - delay: 260ms\n");
    expect(yaml).not.toContain("delay: 1000ms");
  });
});
