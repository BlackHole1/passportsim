import { describe, expect, test } from "bun:test";
import { readdirSync, readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { BUILT_IN_STEPS, REGISTRY_SHAPE, SCENARIO_SCHEMA, STEP_FIELDS } from "./registryShape";
import type { CommandName } from "./commands";

const ROOT = join(dirname(fileURLToPath(import.meta.url)), "..", "..", "..");
const COMMANDS_DIR = join(ROOT, "crates", "pemu-api", "src", "commands");
const SCENARIO_RS = readFileSync(join(ROOT, "crates", "pemu-api", "src", "scenario.rs"), "utf8");

interface RustShape {
  positional: string[];
  scenarioStep: string | null;
}

/** Every `#[command(...)]` attribute in one Rust source, found at any column, closed by bracket depth. */
export function commandAttributes(source: string): Map<string, RustShape> {
  const out = new Map<string, RustShape>();
  const opener = /#\[\s*command\s*\(/g;
  for (let match = opener.exec(source); match !== null; match = opener.exec(source)) {
    let depth = 1;
    let at = match.index + match[0].length;
    const start = at;
    let quote = false;
    for (; at < source.length && depth > 0; at++) {
      const ch = source[at];
      if (quote) {
        if (ch === "\\") at++;
        else if (ch === '"') quote = false;
      } else if (ch === '"') quote = true;
      else if (ch === "(" || ch === "[") depth++;
      else if (ch === ")" || ch === "]") depth--;
    }
    const body = source.slice(start, at - 1);
    const name = /(?:^|[\s,(])name\s*=\s*"([^"]+)"/.exec(body)?.[1];
    if (!name) {
      continue;
    }
    const positional = /\bcli\s*\(\s*positional\s*=\s*\[([^\]]*)\]/.exec(body)?.[1] ?? "";
    out.set(name, {
      positional: [...positional.matchAll(/"([^"]+)"/g)].map((m) => m[1] ?? ""),
      scenarioStep: /\bscenario_step\s*=\s*"([^"]+)"/.exec(body)?.[1] ?? null,
    });
  }
  return out;
}

function rustCommands(): Map<string, RustShape> {
  const out = new Map<string, RustShape>();
  for (const file of readdirSync(COMMANDS_DIR).filter((name) => name.endsWith(".rs"))) {
    for (const [name, shape] of commandAttributes(readFileSync(join(COMMANDS_DIR, file), "utf8"))) {
      out.set(name, shape);
    }
  }
  return out;
}

describe("the attribute reader", () => {
  test("reads an attribute at any column with its fields in any order", () => {
    const source = [
      "mod inner {",
      "    #[command(",
      '        scenario_step = "wait",',
      '        cli( positional = [ "until" ] ),',
      "        errors(E_TIMEOUT),",
      '        name = "run",',
      "    )]",
      "    fn run() {}",
      "}",
      '#[command(name = "stop", cli(positional = ["instance"]))]',
    ].join("\n");
    const shapes = commandAttributes(source);
    expect(shapes.get("run")).toEqual({ positional: ["until"], scenarioStep: "wait" });
    expect(shapes.get("stop")).toEqual({ positional: ["instance"], scenarioStep: null });
  });
});

describe("the registry shape table", () => {
  const rust = rustCommands();

  test("the scan found the registered commands, so the comparison below is not vacuous", () => {
    for (const name of ["input", "env", "run", "snapshot", "ui", "serial"]) {
      expect(rust.has(name)).toBe(true);
    }
  });

  for (const [name, shape] of Object.entries(REGISTRY_SHAPE) as [CommandName, typeof REGISTRY_SHAPE[CommandName]][]) {
    test(`\`${name}\` agrees with its #[command] attribute`, () => {
      const registered = rust.get(name);
      expect(registered !== undefined).toBe(shape.registered);
      if (registered) {
        expect([...shape.positional]).toEqual(registered.positional);
        expect(shape.scenarioStep).toBe(registered.scenarioStep);
      }
    });
  }

  test("the built-in steps, the step fields and the schema string are the runner's", () => {
    const list = (constant: string) =>
      [...(new RegExp(`pub const ${constant}: &\\[&str\\] = &\\[([^\\]]*)\\]`).exec(SCENARIO_RS)?.[1] ?? "").matchAll(/"([^"]+)"/g)].map(
        (m) => m[1],
      );
    expect(list("BUILT_IN_STEPS")).toEqual([...BUILT_IN_STEPS]);
    expect(list("STEP_FIELDS")).toEqual([...STEP_FIELDS]);
    expect(SCENARIO_RS).toContain(`pub const SCHEMA: &str = "${SCENARIO_SCHEMA}";`);
  });
});
