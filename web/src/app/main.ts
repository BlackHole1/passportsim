// The browser entry point: start the Worker, transfer the canvas, hand `page.ts` its transport and
// forward the Worker's messages. A bundle served without the demo boots nothing until a drop.

import { AudioHost, workletUrl } from "../audio/host";
import { CHANNEL_TURN_FLAG, SPIN_WAIT_FLAG, TRACE_FLAG, TRACE_LIMIT, TRACE_LOG, type AudioTraceEntry } from "../audio/trace";
import { workerTransport } from "../api/client";
import type { FromWorker } from "../worker/worker";
import type { RawFrame } from "./capture";
import { daemonAttachPort } from "./daemon";
import { openIndexedDb } from "./history";
import { systemLocale } from "./i18n";
import { createPage, DEMO_IMAGE, type Page } from "./page";
import { PANEL_HEIGHT, PANEL_WIDTH } from "./scale";
import { linkWorker } from "./workerLink";

const DEMO_CONFIG = JSON.stringify({ fw: DEMO_IMAGE });
/** The name the demo's boot carries, so the page can tell its answer from a dropped image's. */
const DEMO_BOOT = "demo";

/** Opens the audio path, or says why not: a browser without an AudioContext still runs the emulator. */
async function openAudio(onMicEnded: () => void): Promise<{ host: AudioHost } | { gap: string }> {
  try {
    return { host: await AudioHost.open(workletUrl(import.meta.url), undefined, onMicEnded) };
  } catch (error) {
    return { gap: error instanceof Error ? error.message : String(error) };
  }
}

/**
 * Whether the bundled demo is served where the Worker looks for it. A GET cancelled after the
 * status, because the daemon's static route answers no HEAD.
 */
async function demoServed(): Promise<boolean> {
  try {
    const response = await fetch(new URL(`./${DEMO_IMAGE}.pebundle`, import.meta.url));
    await response.body?.cancel();
    return response.ok;
  } catch {
    return false;
  }
}

export function start(): Page {
  const mount = document.getElementById("app");
  if (!mount) {
    throw new Error("the page has no #app element");
  }

  // `bun build` does not follow `new Worker(new URL())` into a chunk, so the Worker is its own
  // build entry beside `main.js`.
  const worker = new Worker(new URL("./worker.js", import.meta.url), { type: "module" });
  const scope = globalThis as unknown as Record<string, unknown>;
  const audioTrace = scope[TRACE_FLAG] === true;
  const traceLog: AudioTraceEntry[] = [];
  if (audioTrace) {
    scope[TRACE_LOG] = traceLog;
  }
  let audioHost: AudioHost | null = null;
  let attachAsked = false;
  // Every post notifies the shared input cell, so an isolated Worker's `Atomics.wait` returns.
  const link = linkWorker(worker);
  const transport = workerTransport(link);

  // The canvas is transferred once, on the first `boot` the page posts, so a page that boots no
  // demo still draws its first image.
  let offscreen: OffscreenCanvas | undefined;
  let canvasSent = false;
  const toWorker = (message: unknown, transfer: Transferable[] = []) => {
    const boot = message as { type?: unknown } | null;
    if (!canvasSent && boot?.type === "boot") {
      canvasSent = true;
      const canvas = offscreen;
      link.postMessage(
        {
          ...(message as object),
          canvas,
          canvasSize: { width: PANEL_WIDTH, height: PANEL_HEIGHT },
          ...(audioTrace ? { audioTrace: true } : {}),
          ...(scope[SPIN_WAIT_FLAG] === true ? { spinWait: true } : {}),
          ...(scope[CHANNEL_TURN_FLAG] === true ? { channelTurn: true } : {}),
        },
        [...(canvas ? [canvas] : []), ...transfer],
      );
      return;
    }
    link.postMessage(message, transfer);
  };

  // Frame answers arrive in request order on one channel.
  const frameWaiters: ((frame: RawFrame | null) => void)[] = [];

  const page = createPage({
    mount,
    transport,
    toWorker,
    audioPorts: () => audioHost?.workerPorts() ?? null,
    // Both halves are empty in the browser: snapshots go through the registry, and the panel lives
    // in the Worker behind a transferred canvas.
    rewindSource: {
      snapshot: () => new Uint8Array(0),
      frame: () => null,
    },
    now: () => performance.now(),
    scheduleFrame: (callback) => {
      requestAnimationFrame(() => {
        callback();
      });
    },
    confirm: (message) => globalThis.confirm(message),
    viewport: () => ({ width: window.innerWidth, height: window.innerHeight }),
    devicePixelRatio: () => window.devicePixelRatio,
    hostOs: () => navigator.userAgent,
    // Clipboard exists only in a secure context and may be refused; the Events pane shows the text.
    writeClipboard: async (text) => {
      try {
        await navigator.clipboard.writeText(text);
        return true;
      } catch {
        return false;
      }
    },
    // The `localStorage` getter itself throws when storage is blocked; `prefs.ts` catches it.
    storage: () => window.localStorage,
    search: window.location.search,
    systemLocale,
    readFrame: () =>
      new Promise((resolve) => {
        frameWaiters.push(resolve);
        link.postMessage({ type: "frame" });
      }),
    // Reading `indexedDB` can itself throw where storage is blocked.
    history: () => openIndexedDb(globalThis.indexedDB),
  });

  window.addEventListener("resize", () => {
    page.resize();
  });
  window.addEventListener("keydown", (event) => {
    if (page.key(event, true)) {
      event.preventDefault();
    }
  });
  window.addEventListener("keyup", (event) => {
    page.key(event, false);
  });
  // A release while unfocused never arrives, so held controls are let go on blur and hide.
  window.addEventListener("blur", () => {
    page.releaseControls();
  });
  document.addEventListener("visibilitychange", () => {
    if (document.visibilityState === "hidden") {
      page.releaseControls();
    }
  });

  worker.addEventListener("message", (event: MessageEvent<FromWorker>) => {
    const message = event.data;
    switch (message.type) {
      case "serial":
        page.serial({
          stream: message.stream,
          bytes: message.bytes,
          dropped: BigInt(message.dropped),
          lines: message.lines.map((mark) => ({
            offset: BigInt(mark.offset),
            vtPs: BigInt(mark.vtPs),
          })),
          linesDropped: BigInt(message.linesDropped),
        });
        break;
      case "events":
        page.events(
          message.events.map((event) => ({
            kind: event.kind,
            vtPs: BigInt(event.vtPs),
            arg: BigInt(event.arg),
          })),
        );
        break;
      case "display":
        page.display({ backend: message.backend, reason: message.reason, contextLost: message.contextLost });
        break;
      case "frame": {
        const waiter = frameWaiters.shift();
        waiter?.(message.pixels === null ? null : { width: message.width, height: message.height, pixels: message.pixels });
        break;
      }
      case "audioTrace":
        traceLog.push(message.entry);
        if (traceLog.length > TRACE_LIMIT) {
          traceLog.splice(0, traceLog.length - TRACE_LIMIT);
        }
        break;
      case "ready":
        // Guarded so an audio fault cannot keep `page.ready` from running and strand the load.
        try {
          audioHost?.attach({ audioSab: message.audioSab, captureSab: message.captureSab }, { trace: audioTrace });
        } catch (error) {
          console.error("the audio path could not attach to the new machine:", error);
        }
        page.ready(message.token);
        // A page a daemon served attaches to it once; the Worker's attach follows whatever it runs.
        if (!attachAsked) {
          attachAsked = true;
          void daemonAttachPort(window.location, (path, init) => fetch(path, init)).then((port) => {
            if (port !== null) {
              link.postMessage({ type: "attach", port, label: "web ui" });
            }
          });
        }
        break;
      case "attach":
        if (message.event.kind === "attached") {
          mount.dataset.attached = message.event.instance;
        } else if (message.event.kind === "ended") {
          delete mount.dataset.attached;
        }
        break;
      case "download":
        page.download(
          {
            what: message.what,
            received: message.received,
            total: message.total,
            done: message.done,
            ...(message.error === undefined ? {} : { error: message.error }),
          },
          message.token,
        );
        break;
      case "error":
        page.workerError(message.code ? `${message.code}: ${message.message}` : message.message, message.token);
        break;
      case "fatal":
        page.fatal(message.message, message.code);
        break;
      case "stopped":
        page.stopped(message.stop, message.json);
        break;
      case "stats":
        page.audioOutput(
          message.audio
            ? {
                peak: message.audio.peak,
                pushed: message.audio.pushed,
                quanta: message.audio.playback?.quanta ?? null,
                underruns: message.audio.playback?.underruns ?? null,
                starvedQuanta: message.audio.playback?.starvedQuanta ?? null,
              }
            : null,
        );
        // The panel first, so the repaint `stats` schedules already shows it.
        page.panel(message.panel);
        page.stats({
          mode: "Wall",
          nowPs: BigInt(message.nowPs),
          realTimeFactor: message.realTimeFactor,
          reanchors: message.reanchors,
          sliceVtPs: 0n,
        });
        break;
      default:
        break;
    }
  });

  const canvas = page.canvas;
  offscreen = "transferControlToOffscreen" in canvas ? canvas.transferControlToOffscreen() : undefined;
  // The demo boots after the audio path opens, because the Worker's channel ends travel in `boot`.
  void Promise.all([
    openAudio(() => {
      link.postMessage({ type: "micEnd" });
    }),
    demoServed(),
  ]).then(([opened, demo]) => {
    audioHost = "host" in opened ? opened.host : null;
    if (!demo) {
      page.noDemo();
      return;
    }
    const ports = audioHost?.workerPorts() ?? null;
    toWorker(
      {
        type: "boot",
        config: DEMO_CONFIG,
        token: DEMO_BOOT,
        ...(ports ?? {}),
      },
      ports ? [ports.audioPort, ports.capturePort] : [],
    );
  });

  // An AudioContext starts suspended until a user gesture.
  for (const gesture of ["pointerdown", "keydown"] as const) {
    window.addEventListener(
      gesture,
      () => {
        void audioHost?.resume();
      },
      { once: true },
    );
  }

  (globalThis as { passportEmu?: unknown }).passportEmu = page.automation;
  return page;
}

if (typeof document !== "undefined" && document.getElementById("app")) {
  start();
}
