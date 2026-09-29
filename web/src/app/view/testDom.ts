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
