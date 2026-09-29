// The UI must read the generated `web/src/gen/commands/<command>.d.ts` types rather than keep its
// own copy: this fails as soon as a generated file exists for a command `commands.ts` still declares.

import { describe, expect, test } from "bun:test";
import { readdirSync, readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { COMMAND_GROUP, type CommandName } from "./commands";

const HERE = dirname(fileURLToPath(import.meta.url));
const GEN_COMMANDS = join(HERE, "..", "gen", "commands");
const COMMANDS_SOURCE = readFileSync(join(HERE, "commands.ts"), "utf8");

const DRIVEN = Object.keys(COMMAND_GROUP) as CommandName[];

function generatedCommands(): string[] {
  try {
    return readdirSync(GEN_COMMANDS)
      .filter((name) => name.endsWith(".d.ts"))
      .map((name) => name.slice(0, -".d.ts".length))
      .sort();
  } catch {
    return [];
  }
}

describe("the command table", () => {
  test("every command it drives is a registered one", () => {
    // 13 core commands, then the caps groups. The UI drives none of `debug` (the gdb-side surface) or
    // `device` (native-only, human-confirmed per step).
    const arch82 = new Set([
      "start",
      "stop",
      "status",
      "input",
      "run",
      "serial",
      "ui",
      "screenshot",
      "inspect",
      "snapshot",
      "env",
      "clock",
      "scenario",
      "doctor",
      "audio_capture",
      "mic_set",
      "wifi_ap",
      "net_http",
      "net_capture",
      "ble_scan",
      "ble_connect",
      "ble_gatt",
      "nfc_tag",
      "nfc_tap",
      "power",
      "usb",
    ]);
    for (const name of DRIVEN) {
      expect(arch82.has(name)).toBe(true);
    }
  });

  test("each command's caps group is the registry's", () => {
    expect(COMMAND_GROUP.input).toBe("core");
    expect(COMMAND_GROUP.env).toBe("core");
    expect(COMMAND_GROUP.audio_capture).toBe("audio");
    expect(COMMAND_GROUP.wifi_ap).toBe("radio");
    expect(COMMAND_GROUP.ble_gatt).toBe("radio");
    expect(COMMAND_GROUP.nfc_tap).toBe("nfc");
    expect(COMMAND_GROUP.power).toBe("power");
    expect(COMMAND_GROUP.usb).toBe("power");
  });

  test("a command with a generated type file reads it from ../gen rather than restating it", () => {
    const generated = generatedCommands();
    // Without one generator run the check below would pass vacuously.
    expect(generated).toContain("doctor");
    for (const name of generated) {
      if (!DRIVEN.includes(name as CommandName)) {
        continue;
      }
      expect(COMMANDS_SOURCE).toContain(`from "../gen/commands/${name}"`);
    }
  });

  test("the generated doctor types are re-exported, so a caller needs one import", () => {
    expect(COMMANDS_SOURCE).toContain("export type { DoctorArgs, DoctorResult };");
  });
});
