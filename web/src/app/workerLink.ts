// The page's one door to the Worker. An isolated Worker waits in `Atomics.wait` on the shared
// input cell, so every post is followed by `notifyInput` to end the wait at once rather than
// after the idle sleep. Without the cell, the MessageChannel yielder already lets messages in.

import { notifyInput } from "../worker/pacing";

/** The part of a `Worker` the link uses, so a test can stand in for one. */
export interface WorkerLike {
  postMessage(message: unknown, transfer?: Transferable[]): void;
  addEventListener(type: "message", listener: (event: { data: any }) => void): void;
}

export interface WorkerLink extends WorkerLike {
  readonly inputSab: SharedArrayBuffer | null;
}

export function linkWorker(
  worker: WorkerLike,
  notify: (cell: SharedArrayBuffer) => void = notifyInput,
): WorkerLink {
  let cell: SharedArrayBuffer | null = null;
  worker.addEventListener("message", (event) => {
    const data = event.data as { type?: string; inputSab?: unknown } | null;
    if (data?.type !== "ready") {
      return;
    }
    cell = typeof SharedArrayBuffer !== "undefined" && data.inputSab instanceof SharedArrayBuffer ? data.inputSab : null;
  });
  return {
    get inputSab() {
      return cell;
    },
    postMessage(message, transfer) {
      worker.postMessage(message, transfer ?? []);
      if (cell !== null) {
        notify(cell);
      }
    },
    addEventListener(type, listener) {
      worker.addEventListener(type, listener);
    },
  };
}
