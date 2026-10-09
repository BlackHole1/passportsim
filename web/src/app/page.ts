// The page: the one module that wires the React views (`view/`) to the Worker protocol
// (`web/src/worker/worker.ts`), so every other module is testable without a DOM or a Worker.

import { CommandClient, type CommandTransport } from "../api/client";
import type { ClockArgs, CommandName, SerialChannel, WebCommands } from "../api/commands";
import { defaultShell, type Shell } from "../api/argv";
import { copyAsCli, copyAsStep, type Copied } from "../api/copy";
import { UiJournal } from "../api/journal";
import { ScenarioRecorder, type RecordedScenario, type SessionStart } from "../api/recorder";
import type { DisplayState } from "../gl/sink";
import { toRgba, type PanelView } from "../gl/rgb565";
import type { PacingMode, PacingStats } from "../worker/pacing";
import type { PanelState } from "../worker/session";
import { EventKind } from "../worker/layout";
import { canvasPng, downloadBlob, frameRgba, LIT_PANEL, screenshotName, THUMBNAIL_SCALE, type PngEncoder, type RawFrame } from "./capture";
import { ConsoleModel, type SerialSliceIn } from "./console";
import { ButtonHolds, edgeArgs, pressedAt, type DeviceControl } from "./controls";
import {
  SnapshotController,
  UiTreeController,
  UsbController,
  type CardContext,
} from "./controllers";
import type { DownloadProgress } from "./download";
import { EventLog, type HostEventIn } from "./events";
import { buildIdOf, formatVirtualTime, headerModel, type HeaderModel, type LeaseOwner } from "./header";
import { buildFieldsOf, FirmwareHistory, type HistoryBackend } from "./history";
import type { Locale } from "./i18n";
import { isTextField, keyAction, widgetOwnsKey, type KeyAction } from "./keymap";
import { deviceLayout, type DeviceLayout } from "./layout";
import { DEMO_IMAGE, type LoadedImage } from "./load";
import { createLoader, type Loader } from "./loader";
import type { PlayRelay } from "./play";
import type { AudioOutput } from "./panels/audio";
import { PerfHistory } from "./panels/perf";
import { RewindRing, type RewindSource } from "./panels/snapshots";
import type { GuestRect } from "./panels/uiTree";
import {
  PREF_KEYS,
  initialFollow,
  initialLocale,
  initialMode,
  initialTheme,
  initialZoom,
  safeStorage,
  type Mode,
  type StorageLike,
  type ThemeChoice,
  type Zoom,
} from "./prefs";
import { backlightPercent, GLASS_REGION, type SkinStatus } from "./skin";
import { coreErrorStop, parseStop, type MachineStop } from "./stop";
import { Store, Version } from "./store";
import { TabState } from "./tabs";
import { droppedTap } from "./tap";
import { renderApp } from "./view/App";

/** What the page needs from its host, so a test can supply all of it. */
export interface PageDeps {
  readonly mount: HTMLElement;
  readonly transport: CommandTransport;
  /**
   * Sends a `ToWorker` message that is not a registry command: the pacing mode, posted after the
   * `clock` command was accepted, and the `boot` of a loaded image.
   */
  readonly toWorker: (message: unknown, transfer?: Transferable[]) => void;
  /**
   * Fresh `MessagePort`s for the next machine's audio, or `null` with no audio path. Each boot
   * needs its own pair: the ports are transferred with `boot`, so re-sending old ones throws
   * `DataCloneError`.
   */
  readonly audioPorts?: () => { audioPort: MessagePort; capturePort: MessagePort } | null;
  readonly rewindSource: RewindSource;
  readonly now: () => number;
  readonly confirm: (message: string) => boolean;
  readonly viewport: () => { width: number; height: number };
  readonly devicePixelRatio: () => number;
  /** The OS "Copy as CLI" defaults its shell from; absent means `sh`. */
  readonly hostOs?: () => string | null;
  /** Writes the clipboard; resolves `false` when the context has none. Absent means none. */
  readonly writeClipboard?: (text: string) => Promise<boolean>;
  /**
   * Runs a callback at the display's next frame. The Worker posts `stats` far faster than a display
   * refreshes, so stats-driven views repaint at most once per frame. Absent means no coalescing.
   */
  readonly scheduleFrame?: (callback: () => void) => void;
  readonly schedule?: (callback: () => void, ms: number) => void;
  /** The page's `localStorage`, read on every access (`prefs.ts`); absent means none. */
  readonly storage?: () => StorageLike | null | undefined;
  readonly search?: string;
  readonly systemLocale?: () => string;
  /** A copy of the panel memory from the Worker; `null` with no machine. */
  readonly readFrame?: () => Promise<RawFrame | null>;
  readonly encodePng?: PngEncoder;
  readonly download?: (blob: Blob, name: string) => void;
  /**
   * The relay to the play site on the page's own origin (`play.ts`). Absent, the play box refuses
   * every play as having no relay.
   */
  readonly playRelay?: PlayRelay;
  /** Opens the firmware history's storage. Absent or rejected, the history says it is unavailable. */
  readonly history?: () => Promise<HistoryBackend>;
  readonly wallClock?: () => number;
}

export type CaptureNotice = { readonly kind: "saved"; readonly file: string } | { readonly kind: "failed"; readonly reason: string };

/** Virtual time after a boot at which the history's thumbnail is taken, once the boot has settled. */
export const THUMBNAIL_VT_PS = 3_000_000_000_000n;

export interface Prefs {
  readonly mode: Mode;
  readonly locale: Locale;
  readonly theme: ThemeChoice;
  readonly zoom: Zoom;
  readonly follow: boolean;
}

export interface SkinSnapshot {
  readonly status: SkinStatus;
  readonly display: DisplayState | null;
  /** True while an agent holds the clock or the machine stopped by itself. */
  readonly inputDisabled: boolean;
}

/**
 * Where the machine the page asked for stands until it first runs: the status line and the glass
 * show this, never "paused", while there is no machine yet.
 */
export interface BootView {
  /** A boot was asked for and its machine has not run yet. */
  readonly starting: boolean;
  /** Why that boot failed; the glass offers a retry. */
  readonly failure: string | null;
  /** The download that boot is waiting for, or the last one it finished. */
  readonly download: DownloadProgress | null;
}

/** A box of the guest's screen to outline over the glass (the UI tree tab's hover). */
export interface Highlight {
  readonly rect: GuestRect;
  readonly screen: { readonly w: number; readonly h: number };
}

export interface PageModel {
  readonly ctx: CardContext;
  readonly prefs: Store<Prefs>;
  readonly header: Store<HeaderModel>;
  readonly skin: Store<SkinSnapshot>;
  readonly stop: Store<MachineStop | null>;
  readonly boot: Store<BootView>;
  /** Bumped each time a machine comes up, so what a card learned about the last one is dropped. */
  readonly generation: Store<number>;
  readonly highlight: Store<Highlight | null>;
  readonly taps: Version;
  readonly layout: Store<DeviceLayout>;
  readonly audio: Store<AudioOutput | null>;
  readonly console: ConsoleModel;
  readonly consoleVersion: Version;
  readonly eventLog: EventLog;
  readonly eventsVersion: Version;
  readonly perf: PerfHistory;
  readonly perfVersion: Version;
  readonly tabs: TabState;
  readonly tabsVersion: Version;
  readonly loader: Loader;
  readonly usb: UsbController;
  readonly snapshots: SnapshotController;
  readonly rewind: RewindRing;
  readonly uiTree: UiTreeController;
  readonly history: FirmwareHistory;
  readonly capture: Store<CaptureNotice | null>;
  /** The glass, created once: it is transferred to the Worker and must never be re-created. */
  readonly glass: HTMLCanvasElement;
  readonly rewindCanvas: HTMLCanvasElement;
  readonly actions: PageActions;
}

/** What the views' controls do. Each device and transport action is a registry command. */
export interface PageActions {
  run(): void;
  pause(): void;
  step(): void;
  snapshot(): void;
  /** A ladder button edge: pointer or key down presses, up releases. */
  button(id: "up" | "ok" | "down", down: boolean): void;
  power(down: boolean): void;
  usbState(id: SkinStatus["usb"]): void;
  /** Dropped NDEF-shaped text on the NFC zone: a tap. */
  nfcDrop(text: string): void;
  tapped(): void;
  selectTab(id: string): void;
  toggleCard(id: string): void;
  setMode(mode: Mode): void;
  setLocale(locale: Locale): void;
  setTheme(theme: ThemeChoice): void;
  setZoom(zoom: Zoom): void;
  setFollow(on: boolean): void;
  screenshot(): Promise<void>;
  loadFromHistory(id: string): Promise<void>;
  /** Loads the firmware of the play a typed link or number names, from the play site. */
  loadPlay(text: string): Promise<void>;
  downloadFromHistory(id: string, file?: string): Promise<void>;
  /** Boots the running image again from reset, the way out of a stop that cannot continue. */
  restart(): void;
  /** Boots again what failed to boot: the image whose boot failed, else the demo. */
  retry(): void;
  logRefusal(command: string, error: string): void;
  serialWrite(channel: SerialChannel, text: string): void;
  setConsoleFilter(source: string): void;
  setCapture(on: boolean): void;
  copyCli(journalSeq: number, shell: Shell): Copied;
  copyStep(journalSeq: number): Copied;
  readonly defaultShell: Shell;
  startRecording(): void;
  stopRecording(): RecordedScenario | null;
  writeClipboard(text: string): Promise<boolean>;
}

export interface Page {
  readonly client: CommandClient;
  readonly journal: UiJournal;
  readonly recorder: ScenarioRecorder;
  copyCli(journalSeq: number, shell?: Shell): Copied;
  copyStep(journalSeq: number): Copied;
  readonly console: ConsoleModel;
  readonly rewind: RewindRing;
  readonly perf: PerfHistory;
  readonly canvas: HTMLCanvasElement;
  readonly model: PageModel;
  serial(slice: SerialSliceIn): void;
  stats(stats: PacingStats): void;
  /** The Worker's `stopped`: the loop ended on a panic, tripwire, halt or breakpoint. */
  stopped(code: number, json: string | null): void;
  fatal(message: string, code?: string): void;
  /** The Worker's audio counters; `null` means the machine has no audio path. */
  audioOutput(report: AudioOutput | null): void;
  display(state: DisplayState): void;
  events(events: readonly HostEventIn[]): void;
  readonly eventLog: EventLog;
  resize(): void;
  /** Handles a key event; returns true when the page consumed it. */
  key(event: KeyboardEvent, down: boolean): boolean;
  /** Lets go of every held control: their key or pointer release will not reach a blurred page. */
  releaseControls(): void;
  /** Sets who holds the clock; an agent lease disables the input controls. */
  setLease(owner: LeaseOwner): void;
  panel(state: PanelState): void;
  /**
   * The Worker answered `ready`: the page resumes the machine through the registry, so the resume
   * lands in the journal. While a load waits, a `ready` for a different `token` is the replaced
   * machine finishing its boot and is ignored; one with no token answers whatever is pending.
   */
  ready(token?: string): void;
  /** A Worker-level error. One from a boot the page no longer waits on is ignored as stale. */
  workerError(message: string, token?: string): void;
  /** The Worker's byte counts of a file the boot `token` downloads; a stale boot's are ignored. */
  download(progress: DownloadProgress, token?: string): void;
  readonly loader: Loader;
  /** The bundled demo is not served, so the page waits for a firmware with the controls off. */
  noDemo(): void;
  /** The `window.passportEmu` automation surface. */
  readonly automation: { call: (name: string, args: unknown) => Promise<unknown> };
}

export function createPage(deps: PageDeps): Page {
  const journal = new UiJournal();
  const rewind = new RewindRing();
  const perfHistory = new PerfHistory();
  const consoleModel = new ConsoleModel();
  const eventLog = new EventLog();
  const client = new CommandClient(deps.transport, { journal, nowPs: () => nowPs });
  // Reads the page makes for its own display: nobody asked for them, so the journal a session
  // export replays does not list them.
  const reader = new CommandClient(deps.transport);
  const ctx: CardContext = { client, reader, confirm: deps.confirm };

  const storage = safeStorage(deps.storage ?? (() => null));
  const search = deps.search ?? "";
  const prefs = new Store<Prefs>({
    mode: initialMode(search, storage),
    locale: initialLocale(search, storage, deps.systemLocale?.() ?? ""),
    theme: initialTheme(search, storage),
    zoom: initialZoom(search, storage),
    follow: initialFollow(storage),
  });

  let nowPs = 0n;
  let lease: LeaseOwner = "ui";
  let running = false;
  let image = DEMO_IMAGE;
  /** Bumped by every load, so a call that outlived its machine cannot pace the next ({@link setPace}). */
  let machine = 0;
  let realtimeFor: number | null = null;
  /** A boot waiting for the Worker's `ready` or `error`, keyed by the `token` the Worker echoes. */
  let booting: { token: string; resolve: () => void; reject: (error: Error) => void } | null = null;
  let awaitingConsole = false;
  let empty = false;
  let buildId: string | null = null;
  let panelNow: PanelView = LIT_PANEL;
  let thumbnailFor: { id: string; machine: number } | null = null;

  const makeHeader = (): HeaderModel => {
    const perf = perfHistory.report();
    const model = headerModel({
      image,
      buildId: buildId ?? "--",
      instance: "b1",
      nowPs,
      // `--x` rather than `0.00x`, which would claim a stalled machine.
      realTimeFactor: empty || perf.samples.length === 0 ? Number.NaN : perf.current,
      lease,
      mode: running ? { kind: "Wall", rate: 1 } : { kind: "Paused" },
    });
    return empty ? { ...model, inputDisabled: true } : model;
  };
  const header = new Store<HeaderModel>(makeHeader());
  const skin = new Store<SkinSnapshot>({
    status: { backlightPercent: null, panel: null, usb: "U3" },
    display: null,
    inputDisabled: false,
  });
  const stop = new Store<MachineStop | null>(null);
  // The page boots the demo as soon as it is up, so it starts out starting; `noDemo` says otherwise.
  const boot = new Store<BootView>({ starting: true, failure: null, download: null });
  const generation = new Store(0);
  const highlight = new Store<Highlight | null>(null);
  const taps = new Version();
  const layout = new Store<DeviceLayout>(deviceLayout(prefs.get().mode, deps.viewport(), deps.devicePixelRatio(), prefs.get().zoom));
  const audio = new Store<AudioOutput | null>(null);
  const consoleVersion = new Version();
  const eventsVersion = new Version();
  const perfVersion = new Version();
  const tabs = new TabState();
  const tabsVersion = new Version();

  const glass = document.createElement("canvas");
  glass.className = "glass";
  glass.width = GLASS_REGION.width;
  glass.height = GLASS_REGION.height;
  glass.setAttribute("role", "img");
  const rewindCanvas = document.createElement("canvas");
  rewindCanvas.className = "glass rewind-frame";
  rewindCanvas.width = GLASS_REGION.width;
  rewindCanvas.height = GLASS_REGION.height;
  rewindCanvas.setAttribute("role", "img");
  rewindCanvas.hidden = true;

  const refreshHeader = () => {
    const next = makeHeader();
    header.set(next);
    const disabled = next.inputDisabled || stop.get() !== null;
    if (skin.get().inputDisabled !== disabled) {
      skin.update((current) => ({ ...current, inputDisabled: disabled }));
    }
  };

  const clearStop = () => {
    if (stop.get() !== null) {
      stop.set(null);
      refreshHeader();
    }
  };

  /**
   * Re-paces the machine: the registry first, the Worker only if it accepted. `tryCall`, because a
   * refusal (an agent holds the lease) is a normal answer to a transport button. A machine is built
   * `deterministic`, where `clock --op resume` is refused, so each machine's first resume switches
   * its clock to `realtime`. Each step checks it is still on the machine it started for.
   */
  const setPace = async (op: ClockArgs["op"], worker: PacingMode) => {
    const generation = machine;
    if (op === "resume" && realtimeFor !== generation) {
      if ((await client.tryCall("clock", { op: "set_mode", mode: "realtime" })) === null) {
        return;
      }
      realtimeFor = generation;
    }
    if ((await client.tryCall("clock", { op })) === null) {
      return;
    }
    if (machine !== generation) {
      // A load replaced the machine while this was in flight; its own `ready` paces it.
      return;
    }
    running = worker.kind !== "Paused";
    started();
    if (running) {
      stop.set(null);
    }
    deps.toWorker({ type: "mode", mode: worker });
    refreshHeader();
  };

  /** The machine is up and paced: from here the status says running or paused. */
  const started = () => {
    if (boot.get().starting) {
      boot.update((current) => ({ ...current, starting: false, failure: null }));
    }
  };

  const holds = new ButtonHolds();

  const sendDueReleases = () => {
    for (const control of holds.due(nowPs)) {
      void client.tryCall("input", edgeArgs(control, false));
    }
  };

  /**
   * One edge of a device control. The press is sent at once; the release once the press has lasted
   * `MIN_HOLD_US` of guest time, so a paused machine's release waits for the run.
   */
  const pressControl = (control: DeviceControl, down: boolean) => {
    if (!down) {
      if (holds.letGo(control)) {
        sendDueReleases();
      }
      return;
    }
    if (!holds.press(control)) {
      return;
    }
    const generation = machine;
    void client.tryCall("input", edgeArgs(control, true)).then((output) => {
      if (machine !== generation) {
        return;
      }
      if (output === null) {
        holds.forget(control);
        return;
      }
      holds.pressed(control, pressedAt(output.json) ?? nowPs);
      sendDueReleases();
    });
  };

  /**
   * Replaces the machine: pause through the registry (so an exported journal shows the pause),
   * boot the new one in the Worker, resume on its `ready`. Resolves on that `ready`, not on the
   * post, so every later call reaches the new machine; a refused boot rejects.
   */
  const bootMachine = async (name: string, assets: LoadedImage["assets"] | null): Promise<void> => {
    await client.tryCall("clock", { op: "pause" });
    running = false;
    machine += 1;
    holds.clear();
    const token = `load-${machine}`;
    deps.toWorker({ type: "mode", mode: { kind: "Paused" } });
    const ports = deps.audioPorts?.() ?? null;
    deps.toWorker(
      {
        type: "boot",
        config: JSON.stringify({ fw: name }),
        // No assets is the bundled demo, which the Worker fetches by its `fw` name.
        ...(assets === null ? {} : { assets: assets.map((asset) => ({ kind: asset.kind, bytes: asset.bytes })) }),
        token,
        ...(ports ?? {}),
      },
      ports ? [ports.audioPort, ports.capturePort] : [],
    );
    loader.progress({ kind: "boot", name, assets: assets?.length ?? 0 });
    boot.set({ starting: true, failure: null, download: null });
    image = name;
    buildId = null;
    nowPs = 0n;
    refreshHeader();
    await new Promise<void>((resolve, reject) => {
      booting = { token, resolve, reject };
    });
  };

  const history = new FirmwareHistory(deps.history ?? null, deps.wallClock ?? Date.now);
  const capture = new Store<CaptureNotice | null>(null);
  const encodePng = deps.encodePng ?? canvasPng;
  const download = deps.download ?? downloadBlob;

  const screenPng = async (scale: number): Promise<Blob | null> => {
    const frame = (await deps.readFrame?.()) ?? null;
    const rgba = frame === null ? null : frameRgba(frame, panelNow);
    return frame === null || rgba === null ? null : encodePng(rgba, frame.width, frame.height, scale);
  };

  const keepInHistory = (loaded: LoadedImage) => {
    const of = machine;
    void history.record(loaded).then((id) => {
      if (id === null || of !== machine) {
        return;
      }
      thumbnailFor = { id, machine: of };
      void reader.tryCall("status", {}).then((output) => {
        if (output !== null && of === machine) {
          void history.annotate(id, { buildId: buildIdOf(output.json), ...buildFieldsOf(output.json) });
        }
      });
    });
  };

  const takeThumbnail = (id: string, of: number) => {
    void screenPng(THUMBNAIL_SCALE)
      .then((blob) => (blob === null || of !== machine ? null : blob.arrayBuffer()))
      .then((bytes) => {
        if (bytes !== null && of === machine) {
          void history.annotate(id, { thumbnail: new Uint8Array(bytes) });
        }
      })
      .catch(() => {
      });
  };

  const loader = createLoader(
    {
      onImage: (loaded) => bootMachine(loaded.name, loaded.assets),
      onBooted: keepInHistory,
      onDemo: () => {
        loader.begin({ kind: "demo" });
        return bootMachine(DEMO_IMAGE, null);
      },
      now: deps.now,
      ...(deps.playRelay === undefined ? {} : { playRelay: deps.playRelay }),
    },
    DEMO_IMAGE,
  );
  loader.begin({ kind: "demo" });

  const usb = new UsbController(ctx);
  usb.store.subscribe(() => {
    const id = usb.store.get().id;
    if (skin.get().status.usb !== id) {
      skin.update((current) => ({ ...current, status: { ...current.status, usb: id } }));
    }
  });

  const showFrame = (frame: Uint16Array | null) => {
    if (frame === null) {
      rewindCanvas.hidden = true;
      return;
    }
    const context = rewindCanvas.getContext("2d");
    if (!context) {
      return;
    }
    const data = context.createImageData(GLASS_REGION.width, GLASS_REGION.height);
    // A stored frame carries no backlight state, so it is drawn at full backlight.
    toRgba(frame, OVERLAY_PANEL, data.data);
    context.putImageData(data, 0, 0);
    rewindCanvas.hidden = false;
  };

  const snapshots = new SnapshotController({ ...ctx, ring: rewind, source: deps.rewindSource, showFrame, nowPs: () => nowPs });

  const uiTree = new UiTreeController({
    ...ctx,
    highlight: (rect, screen) => {
      highlight.set(rect === null ? null : { rect, screen });
    },
    now: deps.now,
    schedule:
      deps.schedule ??
      ((callback, ms) => {
        setTimeout(callback, ms);
      }),
  });

  const recorder = new ScenarioRecorder(journal);
  const cliShell = defaultShell(deps.hostOs?.() ?? null);
  const entry = (seq: number) => journal.list().find((record) => record.seq === seq) ?? null;
  const copyCli = (seq: number, chosen: Shell = cliShell): Copied => {
    const record = entry(seq);
    return record === null
      ? { ok: false, reason: `journal entry #${seq} is no longer kept (JOURNAL_LIMIT)` }
      : copyAsCli(record, journal.secrets, chosen);
  };
  const copyStep = (seq: number): Copied => {
    const record = entry(seq);
    return record === null
      ? { ok: false, reason: `journal entry #${seq} is no longer kept (JOURNAL_LIMIT)` }
      : copyAsStep(record, journal.secrets);
  };
  // The Events tab's inputs are the page's own registry calls; resets, panics and frames come
  // from the machine's ring.
  journal.subscribe((record) => {
    if (record.outcome.state === "pending") {
      eventLog.pushCall(record.command, record.args, record.vtPs, record.seq);
      eventsVersion.bump();
    }
  });

  const treeShown = () => prefs.get().mode === "advanced" && tabs.active === "ui-tree";

  const selectTab = (id: string) => {
    if (!tabs.select(id)) {
      return;
    }
    tabsVersion.bump();
    perfVersion.bump();
    uiTree.setVisible(treeShown());
  };

  const resize = () => {
    layout.set(deviceLayout(prefs.get().mode, deps.viewport(), deps.devicePixelRatio(), prefs.get().zoom));
  };

  const setPref = <K extends keyof Prefs>(key: K, value: Prefs[K]) => {
    if (prefs.get()[key] === value) {
      return;
    }
    prefs.update((current) => ({ ...current, [key]: value }));
    storage.write(PREF_KEYS[key], String(value));
  };

  const coalesce = (paint: () => void) => {
    let pending = false;
    return () => {
      if (!deps.scheduleFrame) {
        paint();
        return;
      }
      if (pending) {
        return;
      }
      pending = true;
      deps.scheduleFrame(() => {
        pending = false;
        paint();
      });
    };
  };
  const repaintStats = coalesce(() => {
    perfVersion.bump();
    refreshHeader();
  });
  const repaintConsole = coalesce(() => {
    consoleVersion.bump();
  });
  const showStop = (parsed: MachineStop) => {
    running = false;
    started();
    stop.set(parsed);
    if (parsed.vtPs !== null) {
      nowPs = parsed.vtPs;
    }
    holds.clear();
    loader.progress({ kind: "stopped", stop: parsed, vt: formatVirtualTime(nowPs) });
    refreshHeader();
  };

  const actions: PageActions = {
    // Every control calls a registry command, so the UI journal equals an agent's. The Worker's
    // mode follows the accepted command rather than preceding it.
    run: () => {
      void setPace("resume", { kind: "Wall", rate: 1 });
    },
    pause: () => {
      void setPace("pause", { kind: "Paused" });
    },
    step: () => {
      void client.tryCall("clock", { op: "step", insns: 1 });
    },
    // Through the Snapshots card, which records the id `snapshot restore` needs.
    snapshot: () => {
      void snapshots.save();
    },
    button: (id, down) => {
      pressControl(id, down);
    },
    power: (down) => {
      pressControl("power", down);
    },
    usbState: (id) => {
      void usb.select(id);
    },
    nfcDrop: (text) => {
      void client.tryCall("nfc_tap", droppedTap(text)).then(() => {
        taps.bump();
      });
    },
    tapped: () => {
      taps.bump();
    },
    selectTab,
    toggleCard: (id) => {
      tabs.toggleCard(id, layout.get().narrow);
      tabsVersion.bump();
    },
    setMode: (mode) => {
      setPref("mode", mode);
      resize();
      uiTree.setVisible(treeShown());
    },
    setLocale: (locale) => {
      setPref("locale", locale);
    },
    setTheme: (theme) => {
      setPref("theme", theme);
    },
    setZoom: (zoom) => {
      setPref("zoom", zoom);
      resize();
    },
    setFollow: (on) => {
      setPref("follow", on);
    },
    screenshot: async () => {
      const name = screenshotName(image, nowPs);
      try {
        const blob = empty ? null : await screenPng(1);
        if (blob === null) {
          capture.set({ kind: "failed", reason: empty ? "no firmware is running" : "the screen could not be read" });
          return;
        }
        download(blob, name);
        capture.set({ kind: "saved", file: name });
      } catch (error) {
        capture.set({ kind: "failed", reason: error instanceof Error ? error.message : String(error) });
      }
    },
    loadFromHistory: async (id) => {
      const loaded = await history.image(id);
      if (loaded !== null) {
        await loader.run(loaded);
      }
    },
    loadPlay: (text) => loader.play(text, prefs.get().locale === "zh-CN"),
    downloadFromHistory: async (id, file) => {
      const files = await history.files(id);
      for (const one of files ?? []) {
        if (file === undefined || one.name === file) {
          download(new Blob([one.bytes as Uint8Array<ArrayBuffer>], { type: "application/octet-stream" }), one.name);
        }
      }
    },
    restart: () => {
      void loader.restart();
    },
    retry: () => {
      void loader.retry();
    },
    logRefusal: (command, error) => {
      loader.progress({ kind: "refused", command, error });
    },
    serialWrite: (channel, text) => {
      void client.tryCall("serial", { op: "write", stream: channel, text });
    },
    setConsoleFilter: (source) => {
      consoleModel.setFilter(source);
      consoleVersion.bump();
    },
    setCapture: (on) => {
      consoleModel.setCapture(on);
      consoleVersion.bump();
    },
    copyCli,
    copyStep,
    defaultShell: cliShell,
    startRecording: () => {
      recorder.start(sessionStart(image, skin.get().status.usb));
    },
    stopRecording: () => recorder.stop("recorded-session", nowPs, "A session recorded in the web UI"),
    writeClipboard: deps.writeClipboard ?? (() => Promise.resolve(false)),
  };

  const model: PageModel = {
    ctx,
    prefs,
    header,
    skin,
    stop,
    boot,
    generation,
    highlight,
    taps,
    layout,
    audio,
    console: consoleModel,
    consoleVersion,
    eventLog,
    eventsVersion,
    perf: perfHistory,
    perfVersion,
    tabs,
    tabsVersion,
    loader,
    usb,
    snapshots,
    rewind,
    uiTree,
    history,
    capture,
    glass,
    rewindCanvas,
    actions,
  };

  renderApp(deps.mount, model);
  // The NFC zone's text drop bubbles here too and is ignored, because it carries no file.
  loader.watchDrops(deps.mount);

  return {
    client,
    journal,
    recorder,
    copyCli,
    copyStep,
    console: consoleModel,
    rewind,
    perf: perfHistory,
    canvas: glass,
    model,
    serial(slice) {
      consoleModel.push(slice);
      if (awaitingConsole && slice.bytes.length > 0) {
        awaitingConsole = false;
        loader.progress({ kind: "console" });
      }
      repaintConsole();
    },
    eventLog,
    display(state) {
      skin.update((current) => ({ ...current, display: state }));
    },
    ready(token) {
      if (booting !== null && token !== undefined && booting.token !== token) {
        // The replaced machine finishing its own boot.
        return;
      }
      empty = false;
      if (boot.get().failure !== null) {
        boot.update((current) => ({ ...current, failure: null }));
      }
      // The new machine's byte cursors restart at zero. Cleared here, not when the load is posted,
      // because the Worker is still draining the old machine's serial ring then.
      consoleModel.clear();
      consoleVersion.bump();
      uiTree.reset();
      clearStop();
      usb.reset();
      generation.update((count) => count + 1);
      loader.progress({ kind: "ready", name: image });
      awaitingConsole = true;
      const load = booting;
      booting = null;
      load?.resolve();
      const asked = machine;
      // A refused resume still leaves a machine that is up, and then paused.
      void setPace("resume", { kind: "Wall", rate: 1 }).then(() => {
        if (asked === machine) {
          started();
          refreshHeader();
        }
      });
      void reader.tryCall("status", {}).then((output) => {
        if (asked === machine) {
          buildId = buildIdOf(output?.json);
          refreshHeader();
        }
      });
    },
    workerError(message, token) {
      if (booting !== null && token !== undefined && booting.token !== token) {
        return;
      }
      if (boot.get().starting) {
        boot.update((current) => ({ ...current, failure: message }));
      }
      const load = booting;
      booting = null;
      if (load !== null) {
        load.reject(new Error(message));
        return;
      }
      loader.progress({ kind: "machine-error", detail: message });
      loader.say("error", { kind: "machine", detail: message });
    },
    download(progress, token) {
      if (booting !== null && token !== undefined && booting.token !== token) {
        return;
      }
      if (!boot.get().starting) {
        return;
      }
      boot.update((current) => ({ ...current, download: progress }));
      loader.download(progress);
    },
    loader,
    noDemo() {
      loader.noDemo();
      if (booting !== null || stop.get() !== null || generation.get() > 0) {
        // A dropped image got there first.
        return;
      }
      boot.set({ starting: false, failure: null, download: null });
      empty = true;
      image = "";
      refreshHeader();
    },
    events(events) {
      recorder.observe(events);
      eventLog.pushHost(events);
      eventsVersion.bump();
      // The tree can only change on a presented frame or a settled UI, so the tab reads on those and
      // never on a timer.
      if (events.some((event) => event.kind === EventKind.Frame || event.kind === EventKind.UiSettled)) {
        uiTree.changed();
      }
    },
    stats(stats) {
      nowPs = stats.nowPs;
      if (holds.waiting) {
        sendDueReleases();
      }
      perfHistory.push(deps.now(), stats);
      // Taken at a fixed virtual time so the same firmware gives the same picture.
      if (thumbnailFor !== null && stats.nowPs >= THUMBNAIL_VT_PS) {
        const { id, machine: of } = thumbnailFor;
        thumbnailFor = null;
        if (of === machine) {
          takeThumbnail(id, of);
        }
      }
      if (rewind.maybeCapture(stats.nowPs, deps.rewindSource) !== null) {
        snapshots.refresh();
      }
      repaintStats();
    },
    stopped(code, json) {
      showStop(parseStop(code, json));
    },
    fatal(message, code) {
      showStop(coreErrorStop(message, code));
    },
    audioOutput(report) {
      audio.set(report);
    },
    resize,
    setLease(owner) {
      lease = owner;
      refreshHeader();
    },
    panel(state) {
      panelNow = state;
      const next = statusFromFrame(state);
      const current = skin.get().status;
      if (next.backlightPercent === current.backlightPercent && next.panel === current.panel) {
        return;
      }
      skin.update((snapshot) => ({ ...snapshot, status: { ...snapshot.status, ...next } }));
    },
    key(event, down) {
      if (!down) {
        // The release follows the key whatever holds focus now: the press may predate the change, and a
        // control left down reads as stuck.
        const control = deviceControl(keyAction({ key: event.key }));
        if (control === null || !holds.isDown(control)) {
          return false;
        }
        pressControl(control, false);
        return true;
      }
      const action = keyAction({
        key: event.key,
        repeat: event.repeat,
        ctrlKey: event.ctrlKey,
        metaKey: event.metaKey,
        altKey: event.altKey,
        inTextField: isTextField(event.target) || widgetOwnsKey(event.key, event.target),
      });
      if (action === null) {
        return false;
      }
      if (action.kind === "tab") {
        if (prefs.get().mode !== "advanced") {
          return false;
        }
        if (!event.repeat) {
          tabs.step(action.delta);
          tabsVersion.bump();
          perfVersion.bump();
          uiTree.setVisible(treeShown());
        }
        return true;
      }
      // An auto-repeat is the same press: consumed (an arrow would scroll the page), sends nothing.
      if (event.repeat || skin.get().inputDisabled) {
        return true;
      }
      pressControl(deviceControl(action)!, true);
      return true;
    },
    releaseControls() {
      if (holds.letGoAll().length > 0) {
        sendDueReleases();
      }
    },
    automation: {
      // The same `CommandClient` as every control, so an agent lands in the same journal as a human.
      call: (name: string, args: unknown) =>
        client.call(name as CommandName, args as WebCommands[CommandName]["args"]),
    },
  };
}

export { DEMO_IMAGE };

const OVERLAY_PANEL = {
  backlight: 1,
  backlightScale: 1,
  glassComplement: false,
  powered: true,
  sleeping: false,
  displayOn: true,
} as const;

/**
 * The `image` and `setup` a recording starts from. U1 is the one state with the MCU rail off, so
 * it is `setup.power: off`; `charger` is the `env` word for VBUS with nothing enumerated.
 */
export function sessionStart(image: string | null, usb: SkinStatus["usb"]): SessionStart {
  switch (usb) {
    case "U0":
      return { image, power: "on", usb: "unplugged" };
    case "U1":
      return { image, power: "off", usb: "charger" };
    case "U2":
      return { image, power: "on", usb: "host" };
    case "U3":
      return { image, power: "on", usb: "open" };
  }
}

/**
 * The skin's status line from the panel state. The panel word follows the renderers' order
 * (`rgb565.ts` `panelGain`): no rail, sleep-in, then DISPOFF each leave it black; `inverted` only
 * when the glass shows the complement of memory, never for INVON alone. A scale of 0 means the
 * core models no duty resolution, so the percent is unknown rather than 0.
 */
export function statusFromFrame(frame: PanelState): Omit<SkinStatus, "usb"> {
  return {
    backlightPercent:
      frame.backlightScale > 0 ? backlightPercent(frame.backlight, frame.backlightScale) : null,
    panel: !frame.powered
      ? "off"
      : frame.sleeping
        ? "sleeping"
        : !frame.displayOn
          ? "display off"
          : frame.glassComplement
            ? "inverted"
            : "on",
  };
}

function deviceControl(action: KeyAction): DeviceControl | null {
  return action === null || action.kind === "tab" ? null : action.kind === "power" ? "power" : action.button;
}
