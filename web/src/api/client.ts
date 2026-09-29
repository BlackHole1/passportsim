// The command client every control calls through. No control in `web/src/app/` changes machine
// state any other way, so the UI journal is complete by construction and equals an agent's. The one
// other message, `page.ts`'s `{type:"mode"}`, only tells the local pacing loop what the registry
// already accepted. The transport is injected: the Worker protocol, a test function, or the
// daemon's WebSocket, and no caller can tell which.

import type { CommandName, WebCommands } from "./commands";
import {
  CommandError,
  decodeResponse,
  encodeRequest,
  type CommandOutput,
  type Json,
} from "./envelope";
import type { JournalEntry, UiJournal } from "./journal";

export type CommandTransport = (
  request: string,
) => Promise<{ readonly ok?: string; readonly err?: string }>;

export const CALL_TIMEOUT_MS = 30_000;

export interface ClientOptions {
  readonly journal?: UiJournal;
  /** Virtual time at the moment of the call, in picoseconds, for the journal. */
  readonly nowPs?: () => bigint;
}

/**
 * The page's single door to the registry. Calls are typed by {@link WebCommands} and journaled
 * before they are sent: a call that never answers is still something the user did, and a journal
 * of successes only would replay differently.
 */
export class CommandClient {
  constructor(
    private readonly transport: CommandTransport,
    private readonly options: ClientOptions = {},
  ) {}

  async call<K extends CommandName>(
    name: K,
    args: WebCommands[K]["args"],
  ): Promise<CommandOutput<Json>> {
    const entry = this.options.journal?.record(
      name,
      args as Json,
      this.options.nowPs?.() ?? null,
    );
    try {
      const reply = await this.transport(encodeRequest(name, args));
      const output = decodeResponse(name, reply);
      const json = output.json as { elapsed_vt_us?: unknown } | null;
      const elapsed = typeof json === "object" && json !== null ? json.elapsed_vt_us : undefined;
      entry?.settle({
        ok: true,
        text: output.text,
        elapsedVtUs: typeof elapsed === "number" && Number.isSafeInteger(elapsed) && elapsed >= 0 ? elapsed : null,
      });
      return output;
    } catch (error) {
      const failure =
        error instanceof CommandError
          ? error
          : new CommandError(name, {
              code: "E_INTERNAL",
              message: error instanceof Error ? error.message : String(error),
            });
      entry?.settle({ ok: false, error: failure.body });
      throw failure;
    }
  }

  /**
   * Runs a command and turns a refusal into `null`, for polling controls, where a throw would be an
   * unhandled rejection every frame; the journal still has the entry.
   */
  async tryCall<K extends CommandName>(
    name: K,
    args: WebCommands[K]["args"],
  ): Promise<CommandOutput<Json> | null> {
    try {
      return await this.call(name, args);
    } catch {
      return null;
    }
  }
}

/**
 * A transport over the Worker protocol, one pending map keyed by id. A reply for an unknown id is
 * dropped: after a reboot the old machine's late answers mean nothing.
 */
export function workerTransport(
  worker: {
    postMessage(message: { type: "call"; id: number; request: string }): void;
    addEventListener(
      type: "message",
      listener: (event: { data: { type?: string; id?: number; ok?: string; err?: string } }) => void,
    ): void;
  },
  timeoutMs = CALL_TIMEOUT_MS,
  schedule: (fn: () => void, ms: number) => unknown = (fn, ms) => setTimeout(fn, ms),
): CommandTransport {
  let nextId = 1;
  const pending = new Map<
    number,
    (reply: { readonly ok?: string; readonly err?: string }) => void
  >();
  worker.addEventListener("message", (event) => {
    const data = event.data;
    if (data.type !== "call" || typeof data.id !== "number") {
      return;
    }
    const settle = pending.get(data.id);
    if (!settle) {
      return;
    }
    pending.delete(data.id);
    settle({ ok: data.ok, err: data.err });
  });
  return (request) =>
    new Promise((resolve) => {
      const id = nextId++;
      pending.set(id, resolve);
      schedule(() => {
        if (!pending.delete(id)) {
          return;
        }
        resolve({
          err: JSON.stringify({
            code: "E_TIMEOUT",
            message: `the worker did not answer within ${timeoutMs} ms`,
            retryable: true,
          }),
        });
      }, timeoutMs);
      worker.postMessage({ type: "call", id, request });
    });
}

export type { JournalEntry };
