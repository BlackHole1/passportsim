// "Copy as scenario step": one journaled call as the `scenario@1` step the runner turns back into
// the same call. The key is the command's `CommandSpec::scenario_step`; the value is the argument
// object as a flow mapping. Two exceptions follow how the runner reads them:
//
// - `run {until: M, timeout: T}` is `wait: 'M'` with the step field `timeout: T`; any other `run`
//   argument has no place in that step, so such a call has no step form.
// - `run {for: D}` is the built-in `delay: D`.
//
// Every scalar is written so the reader types it back as the same JSON: integers bare, strings bare
// only when unambiguous, else quoted. A non-integer number has no scenario@1 form (the reader keeps
// `i64` only), so such a call is refused rather than written as a string.

import type { CommandName } from "./commands";
import type { Json } from "./envelope";
import { REGISTRY_SHAPE } from "./registryShape";

export type StepResult =
  | { readonly ok: true; readonly key: string; readonly value: Json; readonly timeout: string | null }
  | { readonly ok: false; readonly reason: string };

/** A plain scalar the reader returns as this very string (`scenario.rs::scalar_of`, `plain`). */
const PLAIN = /^[A-Za-z0-9_][A-Za-z0-9_./+-]*$/;

function readsAsNonString(text: string): boolean {
  return (
    /^(~|null|Null|NULL|true|True|TRUE|false|False|FALSE)$/.test(text) ||
    // Rust `i64::from_str`: an optional sign, then digits.
    /^[+-]?\d+$/.test(text)
  );
}

const ESCAPES: Readonly<Record<string, string>> = {
  "\n": "\\n",
  "\t": "\\t",
  "\r": "\\r",
  "\0": "\\0",
  '"': '\\"',
  "\\": "\\\\",
};

class Unrepresentable extends Error {}

export function yamlFlow(value: Json): string {
  if (value === null) {
    return "null";
  }
  if (typeof value === "boolean") {
    return value ? "true" : "false";
  }
  if (typeof value === "number") {
    if (!Number.isSafeInteger(value)) {
      throw new Unrepresentable(
        `${value} is not an integer, and scenario@1 numbers are i64 only (pemu_api::scenario::Yaml)`,
      );
    }
    return String(value);
  }
  if (typeof value === "string") {
    return yamlString(value);
  }
  if (Array.isArray(value)) {
    return `[${value.map(yamlFlow).join(", ")}]`;
  }
  const entries = Object.entries(value).map(([key, item]) => `${yamlKey(key)}: ${yamlFlow(item)}`);
  return `{${entries.join(", ")}}`;
}

function yamlKey(key: string): string {
  return PLAIN.test(key) && !readsAsNonString(key) ? key : yamlString(key);
}

export function yamlString(text: string): string {
  if (PLAIN.test(text) && !readsAsNonString(text)) {
    return text;
  }
  const control = /[\x00-\x1f\x7f]/;
  if (!control.test(text)) {
    // Single quotes need only a doubled `'`, and the reader's comment and key scanners skip them cleanly.
    return `'${text.replace(/'/g, "''")}'`;
  }
  let out = '"';
  for (const ch of text) {
    const escape = ESCAPES[ch];
    if (escape !== undefined) {
      out += escape;
    } else if (control.test(ch)) {
      throw new Unrepresentable(
        `U+${ch.codePointAt(0)?.toString(16).padStart(4, "0")} has no escape in scenario@1's double-quoted strings`,
      );
    } else {
      out += ch;
    }
  }
  return `${out}"`;
}

function isObject(value: Json): value is { [key: string]: Json } {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

export function stepOf(command: CommandName, args: Json): StepResult {
  const shape = REGISTRY_SHAPE[command];
  if (!shape.registered) {
    return {
      ok: false,
      reason: `\`${command}\` is not a registered command yet, so it has no scenario step`,
    };
  }
  const object = isObject(args) ? args : {};
  let result: StepResult;
  if (command === "run") {
    const { until, timeout, for: duration, ...rest } = object;
    const extra = Object.keys(rest);
    if (typeof until === "string" && duration === undefined && extra.length === 0) {
      if (!/[:(]/.test(until)) {
        return { ok: false, reason: `\`${until}\` is not matcher text, so \`wait\` would not read it as one` };
      }
      if (timeout !== undefined && typeof timeout !== "string" && typeof timeout !== "number") {
        return { ok: false, reason: "`run --timeout` is not a duration" };
      }
      // Read with `Yaml::as_str`, so `timeout: 250` arrives as "250", which `run` reads as 250 ms.
      result = { ok: true, key: "wait", value: until, timeout: timeout === undefined ? null : String(timeout) };
    } else if (duration !== undefined && until === undefined && timeout === undefined && extra.length === 0) {
      result = { ok: true, key: "delay", value: duration, timeout: null };
    } else {
      return {
        ok: false,
        reason: `a \`run\` with ${Object.keys(object).join(", ") || "no arguments"} has no step form: \`wait\` carries only \`until\` and \`timeout\`, \`delay\` only \`for\``,
      };
    }
  } else if (shape.scenarioStep === null) {
    return {
      ok: false,
      reason: `\`${command}\` claims no scenario step (its CommandSpec::scenario_step is None)`,
    };
  } else {
    result = { ok: true, key: shape.scenarioStep, value: object, timeout: null };
  }
  try {
    yamlFlow(result.ok ? result.value : null);
  } catch (error) {
    if (error instanceof Unrepresentable) {
      return { ok: false, reason: error.message };
    }
    throw error;
  }
  return result;
}

/**
 * One step as the block-sequence item a `steps:` list holds, indented by `indent` spaces:
 *
 * ```yaml
 *   - name: ...
 *     press: {button: ok, action: click}
 * ```
 */
export function renderStep(
  step: Extract<StepResult, { ok: true }>,
  options: { readonly name?: string; readonly indent?: number } = {},
): string {
  const pad = " ".repeat(options.indent ?? 0);
  const lines: string[] = [];
  if (options.name !== undefined && options.name !== "") {
    lines.push(`name: ${yamlString(options.name)}`);
  }
  lines.push(`${yamlKey(step.key)}: ${yamlFlow(step.value)}`);
  if (step.timeout !== null) {
    lines.push(`timeout: ${yamlString(step.timeout)}`);
  }
  return lines.map((line, index) => `${pad}${index === 0 ? "- " : "  "}${line}`).join("\n");
}
