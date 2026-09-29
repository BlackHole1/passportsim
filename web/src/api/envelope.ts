// The JSON envelope the page speaks to the registry: the same `{cmd, args}` body goes to `pemu_call`
// in the Worker and to the daemon's WebSocket, with the id outside it. `pemu_call` parses it in
// `crates/pemu-wasm/src/instance.rs`; a change of spelling touches only `encodeRequest` and
// `decodeResponse`.

export type Json = null | boolean | number | string | Json[] | { [key: string]: Json };

/** An artifact a command produced (`pemu_api::ArtifactRef`). */
export interface ArtifactRef {
  /** Always relative to the artifacts root and forward-slashed, on every host. */
  readonly path: string;
  readonly sha256: string;
  readonly media_type: string;
  readonly bytes: number;
}

/** The receipt every response carries; only the fields the UI shows are named. */
export interface Receipt {
  readonly vt_us?: number;
  readonly insns?: number;
  readonly profile?: string;
  readonly efuse?: string;
  readonly tainted?: boolean;
  readonly determinism?: string;
  readonly journal_len?: number;
  /** Fidelity classes this call touched, keyed by class letter. */
  readonly classes_touched?: Record<string, readonly string[]>;
  readonly unmodeled_first_touch?: readonly string[];
  readonly timing_lint?: readonly Json[];
  /** Other commands add fields to the receipt's `extra` map. */
  readonly [key: string]: unknown;
}

/** What a successful command returns (`pemu_api::Output`). */
export interface CommandOutput<T = Json> {
  readonly json: T;
  readonly text: string;
  readonly artifacts?: readonly ArtifactRef[];
  readonly receipt?: Receipt;
  readonly vt_us?: number;
}

/** A failed command (`pemu_api::ApiError`). */
export interface ApiErrorBody {
  readonly code?: string;
  readonly message?: string;
  readonly vt_us?: number;
  /** Retryable errors are the ones the UI offers a retry for. */
  readonly retryable?: boolean;
  readonly hint?: string | null;
  readonly detail?: Json;
  /** The serial tail an error carries as its nearest evidence. */
  readonly serial_tail?: readonly string[];
}

export class CommandError extends Error {
  constructor(
    readonly command: string,
    readonly body: ApiErrorBody,
  ) {
    super(`${command} failed: ${body.code ?? "E_INTERNAL"} ${body.message ?? ""}`.trim());
    this.name = "CommandError";
  }

  get retryable(): boolean {
    return this.body.retryable === true;
  }
}

/** The request body, without the transport's own id. */
export interface CommandRequest {
  readonly cmd: string;
  readonly args: Json;
}

export function encodeRequest(cmd: string, args: unknown): string {
  return JSON.stringify({ cmd, args: (args ?? {}) as Json } satisfies CommandRequest);
}

/**
 * Decodes one answer. `ok` and `err` are separate because `pemu_call`'s `ResultHeader` status
 * already separates them. A body that is not JSON becomes `E_INTERNAL` rather than a `SyntaxError`,
 * so a control handles it like a refusal.
 */
export function decodeResponse<T = Json>(
  cmd: string,
  reply: { readonly ok?: string; readonly err?: string },
): CommandOutput<T> {
  if (reply.err !== undefined) {
    const body = parseBody(reply.err);
    throw new CommandError(
      cmd,
      isObject(body)
        ? (body as ApiErrorBody)
        : { code: "E_INTERNAL", message: truncate(reply.err) },
    );
  }
  if (reply.ok === undefined) {
    throw new CommandError(cmd, {
      code: "E_INTERNAL",
      message: "the command transport answered with neither a result nor an error",
    });
  }
  const parsed = parseBody(reply.ok);
  if (parsed === undefined) {
    throw new CommandError(cmd, {
      code: "E_INTERNAL",
      message: `the command transport answered with non-JSON: ${truncate(reply.ok)}`,
    });
  }
  // An `Output` has a `json` member; anything else is the payload itself, as the fake core answers.
  if (isOutput<T>(parsed)) {
    return parsed;
  }
  return { json: parsed as T, text: "" };
}

function isObject(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function isOutput<T>(value: unknown): value is CommandOutput<T> {
  return isObject(value) && "json" in value;
}

function parseBody(text: string): unknown {
  try {
    return JSON.parse(text) as unknown;
  } catch {
    return undefined;
  }
}

/** Caps a body quoted into an error message. */
function truncate(text: string, limit = 200): string {
  return text.length <= limit ? text : `${text.slice(0, limit)}...`;
}
