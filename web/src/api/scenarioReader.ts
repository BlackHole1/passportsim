// A TypeScript port of the part of `pemu_api::scenario::Yaml::parse` the recorder writes, so the
// tests can read an exported document back. Test support only: nothing under `web/src/app/` imports
// it. It follows `crates/pemu-api/src/scenario.rs` for comment stripping, `key_end`, the flow
// `Cursor`, `scalar_of`, block mappings and `- key: value` sequences, and throws on tabs, block
// literals, anchors and malformed flow. It validates less than the Rust reader, which
// `recorder.test.ts` also runs through the CLI when one is built.

import type { Json } from "./envelope";

interface Line {
  readonly indent: number;
  readonly text: string;
  readonly number: number;
}

/** `scenario.rs::strip_comment`. */
function stripComment(text: string): string {
  let quote: string | null = null;
  for (let index = 0; index < text.length; index++) {
    const ch = text[index];
    if (quote !== null) {
      if (ch === "\\" && quote === '"') {
        index++;
      } else if (ch === quote) {
        quote = null;
      }
    } else if (ch === '"' || ch === "'") {
      quote = ch;
    } else if (ch === "#" && (index === 0 || text[index - 1] === " ")) {
      return text.slice(0, index);
    }
  }
  return text;
}

/** `scenario.rs::key_end`. */
function keyEnd(text: string): number | null {
  let quote: string | null = null;
  let depth = 0;
  for (let index = 0; index < text.length; index++) {
    const ch = text[index];
    if (quote !== null) {
      if (ch === "\\" && quote === '"') {
        continue;
      }
      if (ch === quote) {
        quote = null;
      }
      continue;
    }
    if (ch === '"' || ch === "'") {
      quote = ch;
    } else if (ch === "{" || ch === "[") {
      depth++;
    } else if (ch === "}" || ch === "]") {
      depth = Math.max(0, depth - 1);
    } else if (ch === ":" && depth === 0) {
      const next = text[index + 1];
      if (next === undefined || next === " ") {
        return index;
      }
    }
  }
  return null;
}

/** `scenario.rs::scalar_of`. */
function scalarOf(text: string): Json {
  if (["", "~", "null", "Null", "NULL"].includes(text)) {
    return null;
  }
  if (["true", "True", "TRUE"].includes(text)) {
    return true;
  }
  if (["false", "False", "FALSE"].includes(text)) {
    return false;
  }
  if (/^[+-]?\d+$/.test(text)) {
    const value = BigInt(text);
    if (value >= -(2n ** 63n) && value < 2n ** 63n) {
      return Number(value);
    }
  }
  return text;
}

const ESCAPES: Readonly<Record<string, string>> = { n: "\n", t: "\t", r: "\r", "0": "\0", '"': '"', "\\": "\\", "/": "/" };

/** `scenario.rs::parse_flow` and its `Cursor`. */
export function parseFlow(text: string): Json {
  let at = 0;
  let depth = 0;
  const skip = () => {
    while (text[at] === " ") at++;
  };
  const value = (): Json => {
    const ch = text[at];
    if (ch === undefined) return null;
    if (ch === "{") {
      at++;
      depth++;
      const out: { [key: string]: Json } = {};
      for (;;) {
        skip();
        const c = text[at];
        if (c === undefined) throw new Error("a flow mapping is not closed");
        if (c === "}") {
          at++;
          depth--;
          return out;
        }
        if (c === "," && Object.keys(out).length > 0) {
          at++;
          continue;
        }
        const key = value();
        if (typeof key === "object" && key !== null) throw new Error("a mapping key must be a scalar");
        skip();
        if (text[at] !== ":") throw new Error(`\`${String(key)}\` has no \`:\``);
        at++;
        skip();
        const item = text[at] === "," || text[at] === "}" ? null : value();
        if (String(key) in out) throw new Error(`\`${String(key)}\` appears twice`);
        out[String(key)] = item;
      }
    }
    if (ch === "[") {
      at++;
      depth++;
      const out: Json[] = [];
      for (;;) {
        skip();
        const c = text[at];
        if (c === undefined) throw new Error("a flow sequence is not closed");
        if (c === "]") {
          at++;
          depth--;
          return out;
        }
        if (c === "," && out.length > 0) {
          at++;
          continue;
        }
        out.push(value());
      }
    }
    if (ch === '"') {
      at++;
      let out = "";
      while (at < text.length) {
        const c = text[at++] as string;
        if (c === '"') return out;
        if (c === "\\") {
          const escape = ESCAPES[text[at++] ?? ""];
          if (escape === undefined) throw new Error("an escape scenario@1 does not know");
          out += escape;
        } else {
          out += c;
        }
      }
      throw new Error("a double-quoted string is not closed");
    }
    if (ch === "'") {
      at++;
      let out = "";
      while (at < text.length) {
        const c = text[at++] as string;
        if (c === "'") {
          if (text[at] === "'") {
            at++;
            out += "'";
            continue;
          }
          return out;
        }
        out += c;
      }
      throw new Error("a single-quoted string is not closed");
    }
    if (ch === "&" || ch === "*" || ch === "!") throw new Error("anchors, aliases and tags are refused");
    const start = at;
    while (at < text.length) {
      const c = text[at] as string;
      if (depth > 0) {
        if (c === "," || c === "}" || c === "]") break;
        if (c === ":" && [undefined, " ", ",", "}", "]"].includes(text[at + 1])) break;
      }
      at++;
    }
    return scalarOf(text.slice(start, at).trim());
  };
  skip();
  const out = value();
  skip();
  if (at < text.length) throw new Error(`unexpected \`${text.slice(at).trim()}\` after a complete value`);
  return out;
}

function lines(text: string): Line[] {
  const out: Line[] = [];
  text.split("\n").forEach((raw, index) => {
    if (/^ *\t/.test(raw)) throw new Error(`line ${index + 1}: a tab is not indentation`);
    const indent = raw.length - raw.replace(/^ +/, "").length;
    const body = stripComment(raw.slice(indent)).trimEnd();
    if (body === "") return;
    if (body.endsWith(": |") || body.endsWith(": |-")) throw new Error(`line ${index + 1}: block literals are not ported`);
    out.push({ indent, text: body, number: index + 1 });
  });
  return out;
}

function block(all: Line[], state: { at: number }, indent: number): Json {
  const first = all[state.at];
  if (first === undefined) return null;
  if (first.text === "-" || first.text.startsWith("- ")) {
    const items: Json[] = [];
    while (state.at < all.length && all[state.at]?.indent === indent && all[state.at]?.text.startsWith("- ")) {
      const line = all[state.at] as Line;
      state.at++;
      const rest = line.text.slice(1).trimStart();
      const childIndent = indent + (line.text.length - rest.length);
      if (keyEnd(rest) === null) {
        items.push(parseFlow(rest));
        continue;
      }
      const map = mapping(all, state, childIndent, { ...line, indent: childIndent, text: rest });
      items.push(map);
    }
    return items;
  }
  if (keyEnd(first.text) === null) {
    state.at++;
    return parseFlow(first.text);
  }
  return mapping(all, state, indent, null);
}

function mapping(all: Line[], state: { at: number }, indent: number, head: Line | null): Json {
  const out: { [key: string]: Json } = {};
  const entry = (line: Line) => {
    const split = keyEnd(line.text) as number;
    const key = String(parseFlow(line.text.slice(0, split).trim()));
    const rest = line.text.slice(split + 1).trim();
    let value: Json;
    if (rest === "") {
      const next = all[state.at];
      value = next !== undefined && next.indent > indent ? block(all, state, next.indent) : null;
    } else {
      value = parseFlow(rest);
    }
    if (key in out) throw new Error(`line ${line.number}: \`${key}\` appears twice`);
    out[key] = value;
  };
  if (head !== null) entry(head);
  while (state.at < all.length && all[state.at]?.indent === indent && keyEnd(all[state.at]?.text ?? "") !== null) {
    const line = all[state.at] as Line;
    state.at++;
    entry(line);
  }
  return out;
}

export function parseScenarioYaml(text: string): Json {
  const all = lines(text);
  const state = { at: 0 };
  const out = block(all, state, all[0]?.indent ?? 0);
  if (state.at < all.length) throw new Error(`line ${all[state.at]?.number}: indented less than its document`);
  return out;
}
