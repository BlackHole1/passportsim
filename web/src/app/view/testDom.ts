// A DOM for `bun test`: one shared happy-dom window, installed once per process, whose globals
// React and Base UI read.

import { Window } from "happy-dom";

const GLOBALS = [
  "document",
  "navigator",
  "location",
  "HTMLElement",
  "HTMLInputElement",
  "HTMLButtonElement",
  "HTMLCanvasElement",
  "HTMLSelectElement",
  "HTMLTextAreaElement",
  "Element",
  "Node",
  "Text",
  "DocumentFragment",
  // Not the event classes: Bun's own `EventTarget` accepts only Bun's `Event`.
  "getComputedStyle",
  "ResizeObserver",
  "MutationObserver",
  "IntersectionObserver",
  "requestAnimationFrame",
  "cancelAnimationFrame",
  "matchMedia",
] as const;

let installed: Window | null = null;

/** The longest markup a failed assertion prints for one node. */
const MAX_PRINTED = 2_000;

/**
 * Makes `expect` print a node as its markup. Bun's matcher message otherwise walks every property
 * of the received value, and a happy-dom node reaches the whole window and React's fiber tree: one
 * failing `expect(element).toBeNull()` spent 30 s on its message on macOS and then passed, and on
 * Windows asked for 32 GiB and aborted the run.
 */
function printNodesAsMarkup(window: Window): void {
  const prototype = window.Node.prototype as unknown as Record<symbol, unknown>;
  prototype[Symbol.for("nodejs.util.inspect.custom")] = function (this: { nodeName: string; outerHTML?: string; textContent: string | null }) {
    const text = this.outerHTML ?? `${this.nodeName} ${JSON.stringify(this.textContent ?? "")}`;
    return text.length > MAX_PRINTED ? `${text.slice(0, MAX_PRINTED)}... (${text.length} characters)` : text;
  };
}

export function installDom(): Window {
  if (installed !== null) {
    return installed;
  }
  const window = new Window({ url: "https://passportsim.test/", width: 1280, height: 800 });
  const scope = globalThis as unknown as Record<string, unknown>;
  const source = window as unknown as Record<string, unknown>;
  scope.window = window;
  for (const name of GLOBALS) {
    const value = source[name];
    // Defined rather than assigned: some (`navigator`) are getters on Bun's global.
    Object.defineProperty(globalThis, name, {
      configurable: true,
      writable: true,
      value: typeof value === "function" && /^[a-z]/.test(name) ? (value as () => unknown).bind(window) : value,
    });
  }
  printNodesAsMarkup(window);
  installed = window;
  return window;
}

export function settle(): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, 0));
}

/** Settles until `done` holds or `turns` settles pass: the turns a call chain takes vary by host. */
export async function settleUntil(done: () => boolean, turns = 200): Promise<void> {
  for (let turn = 0; turn < turns && !done(); turn += 1) {
    await settle();
  }
}
