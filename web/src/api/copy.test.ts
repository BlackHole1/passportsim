import { describe, expect, test } from "bun:test";
import { chmodSync, existsSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { edgeArgs } from "../app/controls";
import { cliCall, defaultShell, quote, renderCli } from "./argv";
import { CommandClient } from "./client";
import type { CommandName } from "./commands";
import { copyAsCli, copyAsStep } from "./copy";
import { SecretValues } from "./redact";
import type { Json } from "./envelope";
import { UiJournal, type JournalRecord } from "./journal";
import { parseFlow, parseScenarioYaml } from "./scenarioReader";
import { stepOf, yamlFlow } from "./step";

async function recordedPress(): Promise<{ record: JournalRecord; journal: UiJournal }> {
  const journal = new UiJournal();
  const client = new CommandClient(async () => ({ ok: JSON.stringify({ text: "ok", json: {} }) }), {
    journal,
    nowPs: () => 1_000_000_000n,
  });
  await client.tryCall("input", edgeArgs("ok", true));
  const record = journal.last();
  if (!record) throw new Error("the press was not journaled");
  return { record, journal };
}

function entry(command: CommandName, args: Json, seq = 1): JournalRecord {
  return { seq, command, args, vtPs: 0n, outcome: { state: "ok", text: "", elapsedVtUs: null } };
}

describe("Copy as CLI", () => {
  test("a recorded press of OK renders the exact line, positionals in registry order", async () => {
    const { record, journal } = await recordedPress();
    expect(copyAsCli(record, journal.secrets, "sh")).toEqual({
      ok: true,
      text: "passportsim input ok press",
      redacted: [],
    });
    expect(copyAsCli(record, journal.secrets, "powershell")).toEqual({
      ok: true,
      text: "passportsim input ok press",
      redacted: [],
    });
  });

  test("a measured hold carries its duration as a flag", () => {
    expect(renderCli(cliCall("input", { button: "down", action: "hold", duration: 740 }), "sh")).toBe(
      "passportsim input down hold --duration 740",
    );
  });

  test("a nested argument goes through `--json -`, flags and positionals stay on argv", () => {
    const call = cliCall("env", { battery: { mv: 3900, soc: 80 }, usb: "open" });
    expect(call.argv).toEqual(["passportsim", "env", "--usb", "open", "--json", "-"]);
    expect(call.stdin).toBe('{"battery":{"mv":3900,"soc":80}}');
    expect(renderCli(call, "sh")).toBe(
      `printf '%s\\n' '{"battery":{"mv":3900,"soc":80}}' | passportsim env --usb open --json -`,
    );
    // Windows PowerShell 5.1 pipes to a native command in `$OutputEncoding`, ASCII by default.
    expect(renderCli(call, "powershell")).toBe(
      `$OutputEncoding = [System.Text.UTF8Encoding]::new($false); '{"battery":{"mv":3900,"soc":80}}' | passportsim env --usb open --json -`,
    );
  });

  test("what argv cannot say exactly moves to the document: false, numeric text, arrays, quotes", () => {
    // `inspect`'s positional `what` is an array, so the positional run ends there.
    expect(cliCall("inspect", { what: ["heap"], nvs_values: false })).toEqual({
      argv: ["passportsim", "inspect", "--json", "-"],
      stdin: '{"what":["heap"],"nvs_values":false}',
    });
    // A snapshot named "7" would come back as the number 7 through a union flag; keep it a string.
    expect(cliCall("snapshot", { op: "save", name: "7" })).toEqual({
      argv: ["passportsim", "snapshot", "save", "--json", "-"],
      stdin: '{"name":"7"}',
    });
    expect(cliCall("input", { action: "click", button: "ok" }).argv).toEqual(["passportsim", "input", "ok", "click"]);
    expect(cliCall("serial", { op: "write", text: 'say "hi"', stream: "usj" })).toEqual({
      argv: ["passportsim", "serial", "write", "--stream", "usj", "--json", "-"],
      stdin: '{"text":"say \\"hi\\""}',
    });
    expect(cliCall("clock", { op: "resume", force: true }).argv).toEqual(["passportsim", "clock", "resume", "--force"]);
    expect(cliCall("run", { for: "1.5s", wall_budget_ms: -1 }).argv).toEqual([
      "passportsim",
      "run",
      "--for",
      "1.5s",
      "--wall-budget-ms=-1",
    ]);
  });

  test("a word the shell could expand or read as an option is quoted", () => {
    for (const shell of ["sh", "powershell"] as const) {
      // zsh expands a leading `=` to a command path.
      expect(quote("=foo", shell)).toBe("'=foo'");
      expect(quote("--level=-v1.2", shell)).toBe("'--level=-v1.2'");
      expect(quote("--label==x", shell)).toBe("'--label==x'");
      expect(quote("-v1.2", shell)).toBe("'-v1.2'");
      expect(quote("--json", shell)).toBe("--json");
      expect(quote("-", shell)).toBe("-");
      expect(quote("--for=1.5s", shell)).toBe("--for=1.5s");
    }
    expect(renderCli(cliCall("run", { for: "1.5s", wall_budget_ms: -1 }), "sh")).toBe(
      "passportsim run --for 1.5s '--wall-budget-ms=-1'",
    );
    expect(renderCli(cliCall("snapshot", { op: "save", name: "=menu" }), "sh")).toBe("passportsim snapshot save '=menu'");
  });

  test("quoting: sh escapes ' as '\\'', PowerShell doubles it", () => {
    expect(quote("it's here", "sh")).toBe(`'it'\\''s here'`);
    expect(quote("it's here", "powershell")).toBe(`'it''s here'`);
    expect(quote("serial:/pk_app: ready/", "sh")).toBe(`'serial:/pk_app: ready/'`);
    expect(quote("a,b", "sh")).toBe("a,b");
    expect(quote("a,b", "powershell")).toBe("'a,b'");
  });

  test("PowerShell doubles every character it reads as a single quote, U+2018 to U+201B too", () => {
    // PowerShell treats U+2018 to U+201B as single quotes, so an undoubled one would end the string.
    for (const q of ["\u2018", "\u2019", "\u201a", "\u201b", "'"]) {
      const text = `it${q}s; Remove-Item x; ${q}`;
      expect(quote(text, "powershell")).toBe(`'it${q}${q}s; Remove-Item x; ${q}${q}'`);
      // Inside the quoted word every quote character appears only in doubled runs.
      const inner = quote(text, "powershell").slice(1, -1);
      expect(inner.replace(/(['\u2018-\u201b])\1/g, "")).not.toMatch(/['\u2018-\u201b]/);
    }
    expect(quote("\u2019bare", "powershell")).toBe("'\u2019\u2019bare'");
  });

  test("the default shell is PowerShell only on a Windows host", () => {
    expect(defaultShell("Mozilla/5.0 (Windows NT 10.0; Win64; x64)")).toBe("powershell");
    expect(defaultShell("Mozilla/5.0 (Macintosh; Intel Mac OS X 15_0)")).toBe("sh");
    expect(defaultShell("darwin")).toBe("sh");
    expect(defaultShell(null)).toBe("sh");
  });

  test("a command no package has registered has no line", () => {
    // A journal from another build can name a command this one does not have.
    const copied = copyAsCli(entry("net_bridge" as CommandName, { ssid: "lab" }), new SecretValues(), "sh");
    expect(copied.ok).toBe(false);
    expect(copied.ok === false && copied.reason).toContain("not a registered command");
  });

  const withSh = process.platform === "win32" ? test.skip : test;
  withSh("the sh line hands the program exactly the argv and the document (run through /bin/sh; skipped on win32: no /bin/sh)", () => {
    const dir = mkdtempSync(join(tmpdir(), "pemu-argv-"));
    try {
      // A stand-in `passportsim` that prints what it received: argv NUL-separated, then stdin.
      const stub = join(dir, "passportsim");
      writeFileSync(stub, "#!/bin/sh\nprintf '%s\\0' \"$@\"\nprintf '\\1'\nif [ -t 0 ]; then :; else cat; fi\n");
      chmodSync(stub, 0o755);
      const cases: Array<[CommandName, Json]> = [
        ["serial", { op: "write", text: "it's a $HOME `tick` \\ line", stream: "uart0" }],
        ["serial", { op: "write", text: "two\nlines and 'quotes' and \"doubles\"" }],
        ["env", { mic: { kind: "tone", hz: 440 }, usb: "host" }],
        ["run", { until: "serial:/pk_app: ready/", timeout: "5s" }],
      ];
      for (const [command, args] of cases) {
        const call = cliCall(command, args);
        const line = renderCli(call, "sh");
        const run = Bun.spawnSync(["/bin/sh", "-c", line], {
          env: { PATH: `${dir}:/usr/bin:/bin`, HOME: "/nonexistent" },
          stdin: "ignore",
        });
        expect(run.exitCode).toBe(0);
        const out = run.stdout.toString();
        const [argvText, stdin] = out.split("\u0001");
        expect((argvText ?? "").split("\0").slice(0, -1)).toEqual(call.argv.slice(1));
        expect(call.stdin === null ? "" : JSON.parse(stdin ?? "")).toEqual(call.stdin === null ? "" : JSON.parse(call.stdin));
      }
    } finally {
      rmSync(dir, { recursive: true, force: true });
    }
  });
});

describe("Copy as CLI against the real CLI", () => {
  const ROOT = join(import.meta.dir, "..", "..", "..");
  const bin = process.env.PEMU_CLI ?? join(ROOT, "target", "debug", "passportsim");
  const runnable = process.platform !== "win32" && existsSync(bin);
  const withCli = runnable ? test : test.skip;

  /**
   * Runs a rendered sh line against the built `passportsim` with no instance. The CLI parses and
   * checks the arguments against the schema before looking for an instance, so `E_STATE: no instance
   * is running` means the line was accepted and `E_USAGE` that it was not.
   */
  function runLine(line: string): string {
    const home = mkdtempSync(join(tmpdir(), "pemu-cli-home-"));
    try {
      const run = Bun.spawnSync(["/bin/sh", "-c", line], {
        env: { PATH: `${dirname(bin)}:/usr/bin:/bin`, PASSPORTSIM_HOME: home, HOME: home },
      });
      return run.stderr.toString() + run.stdout.toString();
    } finally {
      rmSync(home, { recursive: true, force: true });
    }
  }

  withCli(`rendered lines parse, assemble and pass the input schema (skipped when ${bin} is not built or on win32)`, () => {
    const calls: Array<[CommandName, Json]> = [
      ["input", { button: "ok", action: "press" }],
      ["input", { button: "power", action: "release" }],
      ["input", { button: "ok", action: "click" }],
      ["input", { button: "down", action: "hold", duration: 740 }],
      ["env", { battery: { mv: 3900, soc: 80 }, usb: "open" }],
      ["serial", { op: "write", text: `it's "quoted" $HOME; =x -y`, stream: "uart0" }],
      ["run", { until: "serial:/pk_app: ready/", timeout: "5s" }],
      ["snapshot", { op: "save", name: "7" }],
      ["clock", { op: "resume", force: true }],
      ["inspect", { what: ["heap"], nvs_values: false }],
    ];
    for (const [command, args] of calls) {
      const line = renderCli(cliCall(command, args), "sh");
      const out = runLine(line);
      expect({ line, out }).toMatchObject({ out: expect.stringContaining("E_STATE: no instance is running") });
    }
    // The negative control: a value the schema refuses comes back as `E_USAGE`.
    expect(runLine(renderCli(cliCall("input", { button: "ok", action: "clik" }), "sh"))).toContain("E_USAGE");
  });
});

describe("Copy as scenario step", () => {
  test("a recorded press of OK renders the exact step, under the key `input` claims", async () => {
    const { record, journal } = await recordedPress();
    expect(copyAsStep(record, journal.secrets)).toEqual({
      ok: true,
      text: "- press: {button: ok, action: press}",
      redacted: [],
    });
  });

  test("`run --until` is `wait` with a step timeout, `run --for` is `delay`", () => {
    expect(copyAsStep(entry("run", { until: "serial:/pk_app: ready/", timeout: "5s" }), new SecretValues())).toEqual({
      ok: true,
      text: "- wait: 'serial:/pk_app: ready/'\n  timeout: 5s",
      redacted: [],
    });
    expect(copyAsStep(entry("run", { for: 250 }), new SecretValues())).toEqual({ ok: true, text: "- delay: 250", redacted: [] });
    const other = copyAsStep(entry("run", { until: "event:reset", stream: "uart0" }), new SecretValues());
    expect(other.ok).toBe(false);
  });

  test("each registered alias is the key, and a command with none says so", () => {
    expect(copyAsStep(entry("env", { usb: "unplugged" }), new SecretValues())).toMatchObject({ text: "- env: {usb: unplugged}" });
    expect(copyAsStep(entry("snapshot", { op: "save", name: "menu" }), new SecretValues())).toMatchObject({
      text: "- snapshot: {op: save, name: menu}",
    });
    expect(copyAsStep(entry("ui", { diff: 1 }), new SecretValues())).toMatchObject({ text: "- ui.snapshot: {diff: 1}" });
    expect(copyAsStep(entry("serial", { op: "write", text: "hi", newline: true }), new SecretValues())).toMatchObject({
      text: "- serial.write: {op: write, text: hi, newline: true}",
    });
    expect(copyAsStep(entry("clock", { op: "pause" }), new SecretValues())).toMatchObject({ ok: false });
    // `wifi_ap` has the alias `wifi.ap`; `clock` above has none.
    expect(copyAsStep(entry("wifi_ap", { ssid: "lab" }), new SecretValues())).toMatchObject({ text: "- wifi.ap: {ssid: lab}" });
    expect(copyAsStep(entry("nfc_tap", { ops: [{ op: "readNdef" }] }), new SecretValues())).toMatchObject({
      text: "- nfc.tap: {ops: [{op: readNdef}]}",
    });
  });

  test("a float has no scenario@1 form and is refused, not stringified", () => {
    expect(stepOf("env", { battery: { mv: 3900.5 } })).toMatchObject({ ok: false });
  });

  test("every scalar reads back through the scenario@1 grammar as the same JSON", () => {
    const values: Json[] = [
      "ok",
      "7",
      "-3",
      "+4",
      "007",
      "true",
      "null",
      "~",
      "on",
      "1.5s",
      "",
      "a: b",
      "#hash",
      "x # not a comment",
      "it's",
      'say "hi"',
      "tab\tand\nnewline and \\ backslash",
      "{braces}, [brackets]",
      "日本語",
      0,
      -42,
      9_007_199_254_740_991,
      true,
      false,
      null,
      [1, "two", [3], { four: 4 }],
      { "odd key": 1, "7": "seven", nested: { deep: ["x, y", "z: w"] } },
    ];
    for (const value of values) {
      expect(parseFlow(yamlFlow(value))).toEqual(value);
      // And inside a mapping, where a plain scalar ends at `,`, `}` and `: `.
      expect(parseFlow(yamlFlow({ v: value, w: 1 }))).toEqual({ v: value, w: 1 });
    }
  });

  test("a copied step pasted under `steps:` reads back as the recorded call", async () => {
    const { record, journal } = await recordedPress();
    const copied = copyAsStep(record, journal.secrets);
    if (!copied.ok) throw new Error(copied.reason);
    const doc = parseScenarioYaml(
      `schema: passportsim/scenario@1\nname: pasted\nsteps:\n${copied.text.replace(/^/gm, "  ")}\n`,
    ) as { steps: Array<Record<string, Json>> };
    expect(doc.steps).toEqual([{ press: { button: "ok", action: "press" } }]);
  });
});
