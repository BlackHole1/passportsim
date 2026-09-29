// The three card states the page itself drives (the USB U-state, the Snapshots save, the UI
// tree's refresh), as controllers with stores, so the page calls them without a view and a test
// drives them without a DOM.

import type { CommandClient } from "../api/client";
import { CommandError } from "../api/envelope";
import * as usb from "./panels/usb";
import {
  RewindRing,
  pointLabel,
  pointName,
  scrubTo,
  snapshotIdFrom,
  type RewindPoint,
  type RewindSource,
} from "./panels/snapshots";
import * as uiTree from "./panels/uiTree";
import { Store } from "./store";

export interface CardContext {
  readonly client: CommandClient;
  /** The same client, unjournaled: reads a card makes for its own display. */
  readonly reader?: CommandClient;
  readonly confirm: (message: string) => boolean;
}

/** A refusal as a card states it: the registry's own code and message, untranslated. */
export function refusalText(error: unknown, withRetry = true): string {
  if (error instanceof CommandError) {
    return `${error.body.code ?? "E_INTERNAL"}: ${error.body.message ?? ""}${withRetry && error.retryable ? " (retryable)" : ""}`;
  }
  return error instanceof Error ? error.message : String(error);
}

export async function attempt<T>(
  run: () => Promise<T>,
  withRetry = true,
): Promise<{ ok: true; value: T } | { ok: false; error: string; cause: unknown }> {
  try {
    return { ok: true, value: await run() };
  } catch (error) {
    return { ok: false, error: refusalText(error, withRetry), cause: error };
  }
}

export type Radio = "ble" | "wifi";

/**
 * Whether a refusal says no module is bound for `radio`: an `E_STATE` whose message says so, since
 * `E_STATE` alone also means other things. `net_capture`'s refusal also covers a guest that has
 * not started Wi-Fi yet.
 */
export function isUnboundRadio(error: unknown, radio: Radio): boolean {
  if (!(error instanceof CommandError) || error.body.code !== "E_STATE") {
    return false;
  }
  const message = error.body.message ?? "";
  return radio === "ble" ? message.includes("no bound BLE module") : message.includes("no bound Wi-Fi module");
}

export interface UsbView {
  readonly id: usb.UsbStateInfoId;
  readonly error: string | null;
}

/**
 * The one U-state implementation of the page: the skin's strip and the card both call
 * {@link UsbController.select}, so `usb.toArgs` diffs from what the machine really took.
 */
export class UsbController {
  private state = usb.DEFAULT_USB;
  readonly store = new Store<UsbView>({ id: usb.usbState(usb.DEFAULT_USB), error: null });

  constructor(private readonly ctx: CardContext) {}

  current(): usb.UsbStateInfoId {
    return usb.usbState(this.state);
  }

  /** A new machine starts plugged into a host with the port open. */
  reset(): void {
    this.state = usb.DEFAULT_USB;
    this.store.set({ id: usb.usbState(this.state), error: null });
  }

  async select(id: usb.UsbStateInfoId): Promise<usb.UsbStateInfoId> {
    const calls = usb.toArgs(this.state, usb.fromUsbState(id, this.state));
    let error: string | null = null;
    // Applied per accepted `input`, so a sequence refused halfway shows the state actually reached.
    for (const call of calls) {
      const done = await attempt(() => this.ctx.client.call("input", call));
      if (!done.ok) {
        error = done.error;
        break;
      }
      this.state = usb.fromArgs(this.state, call);
    }
    this.store.set({ id: usb.usbState(this.state), error: calls.length === 0 ? this.store.get().error : error });
    return usb.usbState(this.state);
  }
}

export interface SnapshotView {
  readonly version: number;
  /** A refusal, or a point the registry never saved and so cannot restore. */
  readonly error: { readonly kind: "refusal"; readonly text: string } | { readonly kind: "unsaved"; readonly point: string } | null;
  readonly label: { readonly text: string; readonly restorable: boolean } | null;
  /** The last save in the registry: its name and virtual instant, `null` until one succeeded. */
  readonly saved: { readonly name: string; readonly at: string; readonly seq: number } | null;
}

export interface SnapshotDeps extends CardContext {
  readonly ring: RewindRing;
  readonly source: RewindSource;
  readonly showFrame: (frame: Uint16Array | null) => void;
  /** Virtual time as the page last heard it; a manual save is stamped with it like an automatic one. */
  readonly nowPs: () => bigint;
}

export class SnapshotController {
  readonly store = new Store<SnapshotView>({ version: 0, error: null, label: null, saved: null });

  constructor(private readonly deps: SnapshotDeps) {}

  /**
   * A point's label, and whether it can be restored. The ring's automatic points never went through
   * `snapshot save`, so the registry has no id for them; the card says so.
   */
  private labelOf(point: RewindPoint): { text: string; restorable: boolean } {
    return { text: pointLabel(point), restorable: this.deps.ring.registryId(point.seq) !== null };
  }

  private setError(error: string | null): void {
    this.store.update((view) => ({ ...view, error: error === null ? null : { kind: "refusal", text: error } }));
  }

  refresh(): void {
    const last = this.deps.ring.latest();
    this.store.update((view) => ({
      ...view,
      version: view.version + 1,
      label: last === null ? null : this.labelOf(last),
    }));
  }

  async save(): Promise<RewindPoint | null> {
    const point = this.deps.ring.capture(this.deps.nowPs(), this.deps.source);
    const done = await attempt(() => this.deps.client.call("snapshot", { op: "save", name: pointName(point) }), false);
    if (done.ok) {
      const id = snapshotIdFrom(done.value.json);
      if (id !== null) {
        this.deps.ring.setRegistryId(point.seq, id);
      }
      this.store.update((view) => ({ ...view, saved: { name: id ?? pointName(point), at: pointLabel(point), seq: point.seq } }));
    }
    this.setError(done.ok ? null : done.error);
    this.refresh();
    return done.ok ? point : null;
  }

  async fork(): Promise<void> {
    // `fork` needs a name for every op but `list`, so the button forks from where the ring is rather
    // than inventing a name.
    const from = this.deps.ring.latest();
    const done = await attempt(
      () => this.deps.client.call("snapshot", { op: "fork", name: from === null ? "fork" : pointName(from) }),
      false,
    );
    this.setError(done.ok ? null : done.error);
  }

  async list(): Promise<void> {
    const done = await attempt(() => this.deps.client.call("snapshot", { op: "list" }), false);
    this.setError(done.ok ? null : done.error);
  }

  scrub(seq: number): void {
    const rewound = scrubTo(this.deps.ring, seq);
    if (!rewound) {
      return;
    }
    this.store.update((view) => ({ ...view, label: this.labelOf(rewound.point) }));
    // The stored frame goes up first: a paused UI may never repaint, and the canvas would show a
    // different moment.
    this.deps.showFrame(rewound.frame);
  }

  /**
   * The thumb was released: restore that point. On `change`, never `input`, or a drag across a full
   * ring would journal a restore per point passed.
   */
  async restore(seq: number): Promise<void> {
    const rewound = scrubTo(this.deps.ring, seq);
    if (!rewound) {
      return;
    }
    const id = this.deps.ring.registryId(rewound.point.seq);
    if (id === null) {
      this.store.update((view) => ({ ...view, error: { kind: "unsaved", point: pointLabel(rewound.point) } }));
      return;
    }
    const done = await attempt(() => this.deps.client.call("snapshot", { op: "restore", name: id }), false);
    this.setError(done.ok ? null : done.error);
  }
}

export interface UiTreeView {
  readonly state: uiTree.UiTreeState;
  readonly error: string | null;
}

export interface UiTreeDeps extends CardContext {
  readonly highlight: (rect: uiTree.GuestRect | null, screen: { w: number; h: number }) => void;
  readonly now: () => number;
  readonly schedule: (fn: () => void, ms: number) => void;
}

/** The UI tree tab: the reads and the hovered row whose box is drawn over the glass. */
export class UiTreeController {
  readonly store = new Store<UiTreeView>({ state: uiTree.EMPTY, error: null });
  readonly follower: uiTree.UiTreeFollower;
  private hovered: string | null = null;

  constructor(private readonly deps: UiTreeDeps) {
    this.follower = new uiTree.UiTreeFollower({
      read: async (args) => {
        try {
          const output = await deps.client.call("ui", args);
          return { ok: true, json: (output.json ?? {}) as uiTree.UiAnswer };
        } catch (error) {
          return error instanceof CommandError
            ? { ok: false, code: error.body.code ?? "E_INTERNAL", message: error.body.message ?? "" }
            : { ok: false, code: "E_INTERNAL", message: error instanceof Error ? error.message : String(error) };
        }
      },
      now: deps.now,
      schedule: deps.schedule,
      onState: (state) => {
        this.store.set({ state, error: null });
        const still = this.hovered === null ? undefined : state.rows.find((row) => row.ref === this.hovered);
        if (still?.rect) {
          deps.highlight(still.rect, state.screen);
        } else if (this.hovered !== null) {
          this.clear();
        }
      },
      onError: (message) => {
        this.store.update((view) => ({ ...view, error: message }));
      },
    });
  }

  hover(ref: string | null): void {
    if (ref === null) {
      this.clear();
      return;
    }
    const row = this.store.get().state.rows.find((one) => one.ref === ref);
    if (row?.rect) {
      this.hovered = ref;
      this.deps.highlight(row.rect, this.store.get().state.screen);
    }
  }

  private clear(): void {
    this.hovered = null;
    this.deps.highlight(null, this.store.get().state.screen);
  }

  setVisible(visible: boolean): void {
    if (!visible) {
      this.clear();
    }
    this.follower.setVisible(visible);
  }

  changed(): void {
    this.follower.changed();
  }

  reset(): void {
    this.clear();
    this.follower.reset();
  }
}
