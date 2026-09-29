// The image loader as page state. Every refusal is stated on the page, never thrown at the
// console. Progress lines are steps that have happened, stamped with the page clock; there is no
// estimated progress. The view mirrors the state as `data-loader-state` and `data-loader-image`
// for Playwright.

import { dropFromTransfer, WALK_LIMIT } from "./drop";
import { carriesAppElf, loadDrop, type Drop, type LoadedImage, type LoadStep } from "./load";
import type { MachineStop } from "./stop";
import { Store } from "./store";

/** Where a load stands; `empty` is a page with no demo that has not been given a firmware yet. */
export type LoaderState = "idle" | "empty" | "loading" | "loaded" | "refused" | "error";

export type LoaderMessage =
  | { readonly kind: "reading" }
  | { readonly kind: "unreadable"; readonly detail: string }
  | { readonly kind: "refused"; readonly reason: string }
  | { readonly kind: "booting"; readonly name: string; readonly notes: readonly string[] }
  | { readonly kind: "not-booted"; readonly name: string; readonly detail: string }
  | { readonly kind: "running"; readonly name: string; readonly notes: readonly string[] }
  | { readonly kind: "machine"; readonly detail: string }
  | { readonly kind: "no-demo" };

export type ProgressStep =
  | LoadStep
  | { readonly kind: "demo" }
  | { readonly kind: "no-demo" }
  | { readonly kind: "history"; readonly name: string }
  | { readonly kind: "boot"; readonly name: string; readonly assets: number }
  | { readonly kind: "ready"; readonly name: string }
  | { readonly kind: "console" }
  | { readonly kind: "failed" }
  | { readonly kind: "stopped"; readonly stop: MachineStop; readonly vt: string }
  | { readonly kind: "machine-error"; readonly detail: string }
  | { readonly kind: "refused"; readonly command: string; readonly error: string };

export interface ProgressLine {
  readonly seq: number;
  readonly atMs: number;
  readonly step: ProgressStep;
}

export interface LoaderSnapshot {
  readonly state: LoaderState;
  readonly image: string;
  readonly message: LoaderMessage | null;
  readonly over: boolean;
  readonly elf: boolean;
  readonly demo: boolean;
  readonly steps: readonly ProgressLine[];
}

export interface LoaderHandlers {
  /** Boots the image. A rejection is shown by the loader, never left unhandled. */
  readonly onImage: (image: LoadedImage) => Promise<void>;
  readonly onDemo: () => Promise<void>;
  readonly now: () => number;
  /** A dropped or history image booted; not called for the demo or a failed boot. */
  readonly onBooted?: (image: LoadedImage) => void;
}

export interface Loader {
  readonly store: Store<LoaderSnapshot>;
  offer(drop: Drop): Promise<void>;
  run(image: LoadedImage): Promise<void>;
  say(state: LoaderState, message: LoaderMessage | null): void;
  progress(step: ProgressStep): void;
  begin(step: ProgressStep): void;
  backToDemo(): Promise<void>;
  noDemo(): void;
  restart(): Promise<void>;
  watchDrops(zone: EventTarget): void;
}

/** Whether no firmware runs: served without the demo and no drop has booted yet. */
export function runsNothing(snapshot: Pick<LoaderSnapshot, "image">): boolean {
  return snapshot.image === "";
}

export const PROGRESS_LIMIT = 64;

export function createLoader(handlers: LoaderHandlers, demoImage: string): Loader {
  const store = new Store<LoaderSnapshot>({
    state: "idle",
    image: demoImage,
    message: null,
    over: false,
    elf: true,
    demo: true,
    steps: [],
  });
  let current: LoadedImage | null = null;
  let seq = 0;
  let startedAt = handlers.now();

  const say = (state: LoaderState, message: LoaderMessage | null): void => {
    store.update((current) => ({ ...current, state, message }));
  };

  const progress = (step: ProgressStep): void => {
    seq += 1;
    const line: ProgressLine = { seq, atMs: Math.max(0, handlers.now() - startedAt), step };
    store.update((current) => ({ ...current, steps: [...current.steps, line].slice(-PROGRESS_LIMIT) }));
  };

  const begin = (step: ProgressStep): void => {
    startedAt = handlers.now();
    store.update((current) => ({ ...current, steps: [] }));
    progress(step);
  };

  const offer = async (drop: Drop): Promise<void> => {
    startedAt = handlers.now();
    store.update((current) => ({ ...current, steps: [] }));
    say("loading", { kind: "reading" });
    let result;
    try {
      result = await loadDrop(drop, progress);
    } catch (error) {
      progress({ kind: "failed" });
      say("refused", { kind: "unreadable", detail: error instanceof Error ? error.message : String(error) });
      return;
    }
    if (!result.ok) {
      progress({ kind: "failed" });
      say("refused", { kind: "refused", reason: result.reason });
      return;
    }
    await boot(result.image);
  };

  const boot = async (image: LoadedImage): Promise<void> => {
    say("loading", { kind: "booting", name: image.name, notes: image.notes });
    try {
      await handlers.onImage(image);
    } catch (error) {
      progress({ kind: "failed" });
      say("error", { kind: "not-booted", name: image.name, detail: error instanceof Error ? error.message : String(error) });
      return;
    }
    current = image;
    store.update((snapshot) => ({ ...snapshot, image: image.name, elf: carriesAppElf(image) }));
    say("loaded", { kind: "running", name: image.name, notes: image.notes });
    handlers.onBooted?.(image);
  };

  const backToDemo = async (): Promise<void> => {
    if (!store.get().demo) {
      return;
    }
    try {
      await handlers.onDemo();
    } catch (error) {
      progress({ kind: "failed" });
      say("error", { kind: "not-booted", name: demoImage, detail: error instanceof Error ? error.message : String(error) });
      return;
    }
    current = null;
    store.update((snapshot) => ({ ...snapshot, image: demoImage, elf: true }));
    say("loaded", { kind: "running", name: demoImage, notes: [] });
  };

  const restart = async (): Promise<void> => {
    if (current === null) {
      await backToDemo();
      return;
    }
    startedAt = handlers.now();
    store.update((snapshot) => ({ ...snapshot, steps: [] }));
    await boot(current);
  };

  const setOver = (over: boolean) => {
    if (store.get().over !== over) {
      store.update((current) => ({ ...current, over }));
    }
  };

  const run = async (image: LoadedImage): Promise<void> => {
    begin({ kind: "history", name: image.name });
    await boot(image);
  };

  return {
    store,
    offer,
    run,
    say,
    progress,
    begin,
    backToDemo,
    noDemo() {
      const idle = store.get().state === "idle";
      store.update((snapshot) => ({ ...snapshot, demo: false, ...(idle ? { image: "" } : {}) }));
      if (!idle) {
        return;
      }
      begin({ kind: "no-demo" });
      say("empty", { kind: "no-demo" });
    },
    restart,
    watchDrops(zone) {
      // Without both handlers the browser navigates to the dropped file, losing the session.
      zone.addEventListener("dragover", (event) => {
        event.preventDefault();
        setOver(true);
      });
      zone.addEventListener("dragleave", (event) => {
        // Moving between children fires a leave on the child; only leaving the page counts.
        const related = (event as DragEvent).relatedTarget as Node | null;
        const inside = related !== null && typeof (zone as Partial<Node>).contains === "function" && (zone as Node).contains(related);
        if (!inside) {
          setOver(false);
        }
      });
      zone.addEventListener("drop", (event) => {
        event.preventDefault();
        setOver(false);
        const transfer = (event as DragEvent).dataTransfer;
        if (!transfer) {
          return;
        }
        void dropFromTransfer(transfer, WALK_LIMIT).then(
          (drop) => {
            // A drop with no file is the NFC zone's NDEF text, which bubbles here too.
            return drop.files.length === 0 ? undefined : offer(drop);
          },
          (error: unknown) => {
            say("refused", { kind: "unreadable", detail: error instanceof Error ? error.message : String(error) });
          },
        );
      });
    },
  };
}
