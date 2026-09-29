// The browser half of `m9.spec.ts`: the built `worker.js` with the real wasm core, booted as a page
// boots it: a transferred OffscreenCanvas, the audio worklets on SAB or MessagePort, `Wall` pacing,
// a journaled button press, a pause, and registry calls through `pemu_call`.

import { AudioHost, workletUrl } from "../src/audio/host";
import type { FromWorker, ToWorker } from "../src/worker/worker";
import { ButtonId, RingId } from "../src/worker/layout";
import { notifyInput } from "../src/worker/pacing";

/**
 * One step of the mid-run pause: a `pemu_call` request and its answer, or `frame`, the Worker's
 * `raw` frame as plain numbers so it crosses `page.evaluate`.
 */
export interface MidCall {
  readonly request: string;
  readonly ok?: string;
  readonly err?: string;
  readonly frame?: { width: number; height: number; generation: string; pixels: number[] };
}

export interface BootResult {
  readonly isolated: boolean;
  readonly abiVersion: number | null;
  /** Whether `ready` carried the shared playback ring (the page is cross-origin isolated). */
  readonly audioSab: boolean;
  readonly inputSab: boolean;
  readonly errors: string[];
  readonly usj: string;
  readonly uart0: string;
  readonly stats: Extract<FromWorker, { type: "stats" }> | null;
  readonly statsCount: number;
  readonly display: unknown;
  readonly calls: Record<string, { ok?: string; err?: string }>;
  readonly midCalls: MidCall[];
  readonly midNowPs: string | null;
  readonly journal: string | null;
  readonly wallMs: number;
}

const sleep = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms));

export interface MidPause {
  readonly atMs: number;
  readonly requests: readonly string[];
}

/**
 * `Wall` 1x for the paced runs; `Paused` for the unpaced ones, advanced only by registry `run` calls
 * at the mid-run pause; `Max` for the pacing regression check.
 */
export type ProbeMode =
  | { readonly kind: "Wall"; readonly rate: number }
  | { readonly kind: "Max" }
  | { readonly kind: "Paused" };

/**
 * Boots `config`, paces it at `mode` for `wallMs`, presses Ok at `pressAtMs` (held 150 ms of virtual
 * time), pauses, and asks `requests`. With `mid`, it also pauses at `mid.atMs`, asks `mid.requests`
 * one after another, and resumes.
 */
async function boot(
  config: string,
  wallMs: number,
  pressAtMs: number | null,
  requests: readonly string[],
  mid: MidPause | null = null,
  mode: ProbeMode = { kind: "Wall", rate: 1 },
): Promise<BootResult> {
  const isolated = globalThis.crossOriginIsolated === true;
  const errors: string[] = [];
  const decoder = { usj: new TextDecoder(), uart0: new TextDecoder() };
  let usj = "";
  let uart0 = "";
  let stats: Extract<FromWorker, { type: "stats" }> | null = null;
  let statsCount = 0;
  let display: unknown = null;
  let journal: string | null = null;
  let ready: Extract<FromWorker, { type: "ready" }> | null = null;
  const answers = new Map<number, { ok?: string; err?: string }>();
  const midCalls: MidCall[] = [];
  let frameAnswer: Extract<FromWorker, { type: "frame" }> | null = null;
  let midNowPs: string | null = null;

  const host = await AudioHost.open(workletUrl(location.href));
  const ports = host.workerPorts();
  const worker = new Worker(new URL("./worker.js", location.href), { type: "module" });
  worker.addEventListener("error", (event) => errors.push(`uncaught: ${event.message}`));
  worker.addEventListener("message", (event: MessageEvent<FromWorker>) => {
    const message = event.data;
    switch (message.type) {
      case "ready":
        ready = message;
        break;
      case "error":
        errors.push(`${message.code ?? ""} ${message.message}`.trim());
        break;
      case "serial":
        if (message.stream === RingId.UsjTx) {
          usj += decoder.usj.decode(message.bytes, { stream: true });
        } else if (message.stream === RingId.Uart0Tx) {
          uart0 += decoder.uart0.decode(message.bytes, { stream: true });
        }
        break;
      case "stats":
        stats = message;
        statsCount += 1;
        break;
      case "display":
        display = { backend: message.backend, reason: message.reason };
        break;
      case "call":
        answers.set(message.id, { ok: message.ok, err: message.err });
        break;
      case "journal":
        journal = message.json;
        break;
      case "frame":
        frameAnswer = message;
        break;
      default:
        break;
    }
  });
  // After `ready`, every message also notifies the shared input cell, as a page does, so an isolated
  // Worker's `Atomics.wait` returns.
  let inputCell: SharedArrayBuffer | undefined;
  const post = (message: ToWorker, transfer: Transferable[] = []) => {
    worker.postMessage(message, transfer);
    if (inputCell) {
      notifyInput(inputCell);
    }
  };

  const canvas = document.createElement("canvas");
  canvas.width = 240;
  canvas.height = 320;
  document.body.append(canvas);
  const offscreen = canvas.transferControlToOffscreen();
  post({ type: "boot", config, canvas: offscreen, ...ports }, [offscreen, ports.audioPort, ports.capturePort]);
  const bootDeadline = performance.now() + 20_000;
  while (!ready && errors.length === 0 && performance.now() < bootDeadline) {
    await sleep(20);
  }
  const booted = ready as Extract<FromWorker, { type: "ready" }> | null;
  if (booted) {
    host.attach({ audioSab: booted.audioSab, captureSab: booted.captureSab });
    inputCell = booted.inputSab;
    await host.resume();
    const start = performance.now();
    post({ type: "mode", mode });
    let pressed = pressAtMs === null;
    let midDone = mid === null;
    let nextId = 1000;
    while (performance.now() - start < wallMs) {
      if (!midDone && mid && performance.now() - start >= mid.atMs) {
        post({ type: "mode", mode: { kind: "Paused" } });
        await sleep(300);
        midNowPs = (stats as Extract<FromWorker, { type: "stats" }> | null)?.nowPs ?? null;
        for (const request of mid.requests) {
          if (request === "frame") {
            frameAnswer = null;
            post({ type: "frame" });
            const deadline = performance.now() + 10_000;
            while (frameAnswer === null && performance.now() < deadline) {
              await sleep(20);
            }
            const got = frameAnswer as Extract<FromWorker, { type: "frame" }> | null;
            midCalls.push(
              got?.pixels
                ? {
                    request,
                    frame: {
                      width: got.width,
                      height: got.height,
                      generation: got.generation,
                      pixels: Array.from(got.pixels),
                    },
                  }
                : { request, err: got ? "no machine is booted" : "no frame within 10 s" },
            );
            continue;
          }
          const id = nextId++;
          post({ type: "call", id, request });
          const deadline = performance.now() + 10_000;
          while (!answers.has(id) && performance.now() < deadline) {
            await sleep(20);
          }
          midCalls.push({ request, ...(answers.get(id) ?? { err: "no answer within 10 s" }) });
        }
        post({ type: "mode", mode });
        midDone = true;
      }
      if (!pressed && performance.now() - start >= (pressAtMs ?? 0)) {
        // Held in virtual time, not wall time: the Worker stamps each edge when it reads the message, so a
        // wall-timed hold can collapse below the firmware's debounce.
        const current = () => BigInt((stats as Extract<FromWorker, { type: "stats" }> | null)?.nowPs ?? "0");
        const downAt = current();
        post({ type: "button", id: ButtonId.Ok, down: true });
        const holdDeadline = performance.now() + 3_000;
        while (current() < downAt + 150_000_000_000n && performance.now() < holdDeadline) {
          await sleep(10);
        }
        post({ type: "button", id: ButtonId.Ok, down: false });
        pressed = true;
      }
      await sleep(25);
    }
    post({ type: "mode", mode: { kind: "Paused" } });
    await sleep(300);
    requests.forEach((request, index) => post({ type: "call", id: index + 1, request }));
    post({ type: "journal" });
    const callDeadline = performance.now() + 10_000;
    while ((answers.size < requests.length || journal === null) && performance.now() < callDeadline) {
      await sleep(20);
    }
    const calls: Record<string, { ok?: string; err?: string }> = {};
    requests.forEach((request, index) => {
      calls[request] = answers.get(index + 1) ?? { err: "no answer within 10 s" };
    });
    worker.terminate();
    await host.close();
    return {
      isolated,
      abiVersion: booted.abiVersion,
      audioSab: booted.audioSab !== undefined,
      inputSab: booted.inputSab !== undefined,
      errors,
      usj,
      uart0,
      stats,
      statsCount,
      display,
      calls,
      midCalls,
      midNowPs,
      journal,
      wallMs: performance.now() - start,
    };
  }
  worker.terminate();
  await host.close();
  return {
    isolated,
    abiVersion: null,
    audioSab: false,
    inputSab: false,
    errors: errors.length > 0 ? errors : ["no ready within 20 s"],
    usj,
    uart0,
    stats,
    statsCount,
    display,
    calls: {},
    midCalls,
    midNowPs,
    journal,
    wallMs: 0,
  };
}

(globalThis as unknown as { m9Probe: unknown }).m9Probe = { boot };
