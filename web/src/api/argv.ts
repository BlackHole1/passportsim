// "Copy as CLI": one journaled call as the `passportsim` line that makes it, first as an argv
// array, then rendered for one shell. The argv inverts `crates/pemu-cli/src/args.rs`, so
// `assemble` of the line gives back exactly the recorded arguments:
//
// - `CommandSpec::cli.positional` properties become bare words in declared order, until the first
//   one that cannot be one (a later bare word would fill the earlier slot);
// - a scalar becomes `--flag value` (underscores dashed), `true` the bare `--flag`;
// - anything else goes into one `--json -` document on stdin, which flags override: objects and
//   arrays; `false` (whether `--no-<flag>` exists depends on a schema default the page lacks); a
//   string that reads as a number (a union property is typed by what its text parses as); an
//   unsafe integer; and a string that is empty or holds `"` or a line break.
//
// Rendering: `sh` single-quotes (`'` as `'\''`); PowerShell single-quotes with `'` and U+2018 to
// U+201B doubled. A word stays bare only if no shell treats any of its characters specially and it
// (or a flag's value) does not start with `=` or `-`.

import type { CommandName } from "./commands";
import type { Json } from "./envelope";
import { REGISTRY_SHAPE } from "./registryShape";

export type Shell = "sh" | "powershell";

/** The program name every usage line carries. */
export const PROGRAM = "passportsim";

export interface CliCall {
  readonly argv: readonly string[];
  readonly stdin: string | null;
}

type JsonObject = { [key: string]: Json };

function needsDocument(value: Json): boolean {
  if (value === null || typeof value === "object") {
    return true;
  }
  if (value === false) {
    return true;
  }
  if (typeof value === "number") {
    return !Number.isSafeInteger(value);
  }
  if (typeof value === "string") {
    // Windows PowerShell 5.1 strips embedded double quotes from a native command's argument and drops
    // an empty one, so such a string travels in the document on every shell.
    if (value === "" || value.includes('"') || /[\r\n\0]/.test(value)) {
      return true;
    }
    // `args.rs::scalar`'s union branch: i64, then f64. `inf` and `nan` parse as f64 in Rust too.
    return /^[+-]?(\d+\.?\d*([eE][+-]?\d+)?|\.\d+([eE][+-]?\d+)?|inf|infinity|nan)$/i.test(value);
  }
  return false;
}

export function cliCall(command: CommandName, args: Json): CliCall {
  const argv: string[] = [PROGRAM, command];
  const object: JsonObject =
    typeof args === "object" && args !== null && !Array.isArray(args) ? { ...args } : {};
  const document: JsonObject = {};

  let positionalOpen = true;
  for (const name of REGISTRY_SHAPE[command].positional) {
    if (!(name in object)) {
      positionalOpen = false;
      continue;
    }
    const value = object[name] as Json;
    // A bare word cannot start with `-` (clap would read a flag) and cannot be a switch.
    const bare =
      positionalOpen &&
      !needsDocument(value) &&
      typeof value !== "boolean" &&
      !String(value).startsWith("-");
    if (bare) {
      argv.push(String(value));
    } else {
      positionalOpen = false;
      document[name] = value;
    }
    delete object[name];
  }

  for (const [name, value] of Object.entries(object)) {
    if (needsDocument(value)) {
      document[name] = value;
      continue;
    }
    const flag = `--${name.replace(/_/g, "-")}`;
    if (value === true) {
      argv.push(flag);
      continue;
    }
    const text = String(value);
    // `--rssi=-60`: the `=` form keeps a leading `-` from being read as the next flag.
    if (text.startsWith("-")) {
      argv.push(`${flag}=${text}`);
    } else {
      argv.push(flag, text);
    }
  }

  const hasDocument = Object.keys(document).length > 0;
  if (hasDocument) {
    argv.push("--json", "-");
  }
  return { argv, stdin: hasDocument ? JSON.stringify(document) : null };
}

const BARE_SH = /^[A-Za-z0-9_./:=+,-]+$/;
/** PowerShell also builds an array from `,`, so a word with one is quoted there. */
const BARE_PS = /^[A-Za-z0-9_./:=+-]+$/;
const FLAG = /^--[a-z0-9][a-z0-9-]*(=(.*))?$/;

function bareOk(word: string, shell: Shell): boolean {
  if (word.length === 0 || !(shell === "sh" ? BARE_SH : BARE_PS).test(word)) {
    return false;
  }
  if (word === "-") {
    return true;
  }
  const flag = FLAG.exec(word);
  // A value must not start with `=` (zsh expands `=cmd` to a path) or `-` (read as an option).
  const value = flag ? (flag[2] ?? "x") : word;
  return value.length > 0 && !/^[=-]/.test(value);
}

export function quote(word: string, shell: Shell): string {
  if (bareOk(word, shell)) {
    return word;
  }
  // PowerShell's single-quote set is ' and U+2018 to U+201B; each must be doubled as itself.
  return shell === "sh" ? `'${word.replace(/'/g, `'\\''`)}'` : `'${word.replace(/['\u2018-\u201b]/g, "$&$&")}'`;
}

export function renderCli(call: CliCall, shell: Shell): string {
  const words = call.argv.map((word) => quote(word, shell)).join(" ");
  if (call.stdin === null) {
    return words;
  }
  return shell === "sh"
    ? `printf '%s\\n' ${quote(call.stdin, shell)} | ${words}`
    : // Windows PowerShell 5.1 encodes what it pipes to a native command with `$OutputEncoding`,
      // ASCII by default, which turns non-ASCII into `?`; the CLI reads UTF-8 without a BOM. Not yet run
      // in a real PowerShell.
      `$OutputEncoding = [System.Text.UTF8Encoding]::new($false); ${quote(call.stdin, shell)} | ${words}`;
}

/**
 * The shell a line defaults to: PowerShell on a Windows host, `sh` elsewhere. `status` reports no
 * OS yet, so the caller passes the daemon's OS when attached, else the browser's user agent.
 */
export function defaultShell(hostOs: string | null): Shell {
  return hostOs !== null && /win/i.test(hostOs) && !/darwin/i.test(hostOs) ? "powershell" : "sh";
}
