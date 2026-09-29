import { describe, expect, test } from "bun:test";

import { MIC_CONSTRAINTS } from "./capture";
import {
  AudioHost,
  refusalOf,
  workletUrl,
  type AudioHostDeps,
  type ContextLike,
  type StreamLike,
  type TrackLike,
  type WorkletNodeLike,
} from "./host";
import { CAPTURE_PROCESSOR, PLAYBACK_PROCESSOR } from "./worklet";

class FakeNode implements WorkletNodeLike {
  readonly posted: { message: unknown; transfer: Transferable[] }[] = [];
  readonly connections: unknown[] = [];
  disconnected = 0;
  readonly port = {
    postMessage: (message: unknown, transfer: Transferable[] = []) => {
      this.posted.push({ message, transfer });
    },
  };

  constructor(
    readonly name: string,
    readonly options: AudioWorkletNodeOptions,
  ) {}

  connect(destination: unknown): unknown {
    this.connections.push(destination);
    return destination;
  }

  disconnect(): void {
    this.disconnected += 1;
  }
}

class FakeTrack implements TrackLike {
  stopped = false;
  private listeners: (() => void)[] = [];
  constructor(readonly label: string) {}
  stop(): void {
    this.stopped = true;
  }
  addEventListener(_type: "ended", listener: () => void): void {
    this.listeners.push(listener);
  }
  end(): void {
    for (const listener of this.listeners) {
      listener();
    }
  }
}

function fakeBrowser(getUserMedia: AudioHostDeps["getUserMedia"]) {
  const nodes: FakeNode[] = [];
  const sources: { stream: StreamLike; connected: unknown[]; disconnected: boolean }[] = [];
  const loaded: string[] = [];
  let state = "suspended";
  const context: ContextLike = {
    sampleRate: 44_100,
    get state() {
      return state;
    },
    destination: "speakers",
    addModule: async (url) => {
      loaded.push(url);
    },
    createWorkletNode: (name, options) => {
      const node = new FakeNode(name, options);
      nodes.push(node);
      return node;
    },
    createMediaStreamSource: (stream) => {
      const source = { stream, connected: [] as unknown[], disconnected: false };
      sources.push(source);
      return {
        connect: (node: unknown) => source.connected.push(node),
        disconnect: () => {
          source.disconnected = true;
        },
      };
    },
    resume: async () => {
      state = "running";
    },
    close: async () => {
      state = "closed";
    },
  };
  const channels: MessageChannel[] = [];
  const deps: AudioHostDeps = {
    createContext: () => context,
    createChannel: () => {
      const channel = new MessageChannel();
      channels.push(channel);
      return channel;
    },
    getUserMedia,
  };
  return { deps, nodes, sources, loaded, channels, context };
}

function granted(track = new FakeTrack("Fake Default Audio Input")) {
  const requests: MediaStreamConstraints[] = [];
  const getUserMedia = async (constraints: MediaStreamConstraints): Promise<StreamLike> => {
    requests.push(constraints);
    return { getAudioTracks: () => [track] };
  };
  return { getUserMedia, requests, track };
}

async function attachedHost(deps: AudioHostDeps, rings = {}, onEnded = () => {}) {
  const host = await AudioHost.open("http://127.0.0.1/emu/worklet.js", deps, onEnded);
  host.workerPorts();
  host.attach(rings);
  return host;
}

describe("the audio graph", () => {
  test("loads the worklet beside the page script, wherever the bundle is served", () => {
    expect(workletUrl("http://127.0.0.1:4173/emu/main.js")).toBe("http://127.0.0.1:4173/emu/worklet.js");
  });

  test("puts playback on the speakers and hands each node its shared ring and its port", async () => {
    const browser = fakeBrowser(null);
    const host = await AudioHost.open("u/worklet.js", browser.deps);
    const ports = host.workerPorts();
    const audioSab = new SharedArrayBuffer(64);
    const captureSab = new SharedArrayBuffer(64);
    host.attach({ audioSab, captureSab });

    expect(browser.loaded).toEqual(["u/worklet.js"]);
    const [playback, capture] = browser.nodes;
    expect(playback?.name).toBe(PLAYBACK_PROCESSOR);
    expect(playback?.connections).toEqual(["speakers"]);
    expect(playback?.posted[0]?.message).toEqual({ sab: audioSab, guestRate: 16_000 });
    expect(playback?.posted[0]?.transfer).toEqual([browser.channels[0]?.port2 as MessagePort]);
    expect(ports.audioPort).toBe(browser.channels[0]?.port1 as MessagePort);

    expect(capture?.name).toBe(CAPTURE_PROCESSOR);
    expect(capture?.options.numberOfOutputs).toBe(0);
    // The capture node is not wired to the speakers: the mic must never be heard.
    expect(capture?.connections).toEqual([]);
    expect(capture?.posted[0]?.message).toEqual({ sab: captureSab });
    expect(capture?.posted[0]?.transfer).toEqual([browser.channels[1]?.port2 as MessagePort]);
    expect(host.contextRate).toBe(44_100);
  });

  test("without cross-origin isolation the nodes get only the port, which carries the PCM", async () => {
    const browser = fakeBrowser(null);
    await attachedHost(browser.deps, {});
    expect(browser.nodes.map((node) => node.posted[0]?.message)).toEqual([
      { sab: undefined, guestRate: 16_000 },
      { sab: undefined },
    ]);
  });

  // A drop can boot before the demo's `ready`, minting a second port pair while the first is in
  // flight. With one slot, both `ready`s attached the same pair and the second transfer threw
  // `Port at index 0 is already neutered`, so the load never resolved.
  test("two boots in flight give each machine its own pair, oldest attached first", async () => {
    const browser = fakeBrowser(null);
    const host = await AudioHost.open("u/worklet.js", browser.deps);

    const first = host.workerPorts();
    const second = host.workerPorts();
    expect(first.audioPort).not.toBe(second.audioPort);

    host.attach({});
    host.attach({});

    // Four channels in mint order; each node got its own machine's `port2`, and no port twice.
    const transferred = browser.nodes.map((node) => node.posted[0]?.transfer?.[0]);
    expect(transferred).toEqual([
      browser.channels[0]?.port2 as MessagePort,
      browser.channels[1]?.port2 as MessagePort,
      browser.channels[2]?.port2 as MessagePort,
      browser.channels[3]?.port2 as MessagePort,
    ]);
    expect(new Set(transferred).size).toBe(4);
    expect(first.audioPort).toBe(browser.channels[0]?.port1 as MessagePort);
    expect(second.audioPort).toBe(browser.channels[2]?.port1 as MessagePort);
  });

  test("a third `ready` with no pair left refuses instead of re-transferring one", async () => {
    const browser = fakeBrowser(null);
    const host = await AudioHost.open("u/worklet.js", browser.deps);
    host.workerPorts();
    host.attach({});
    expect(() => host.attach({})).toThrow("workerPorts");
  });

  test("refuses to attach before the Worker has been given its ports", async () => {
    const host = await AudioHost.open("w.js", fakeBrowser(null).deps);
    expect(() => host.attach({})).toThrow("workerPorts");
  });

  test("resumes a suspended context once", async () => {
    const browser = fakeBrowser(null);
    const host = await attachedHost(browser.deps);
    await host.resume();
    expect(browser.context.state).toBe("running");
  });
});

describe("the live microphone", () => {
  test("asks for one channel without browser DSP and connects it to the capture node only", async () => {
    const mic = granted();
    const browser = fakeBrowser(mic.getUserMedia);
    const host = await attachedHost(browser.deps);

    const started = await host.startMicrophone();
    expect(started).toEqual({ ok: true, label: "Fake Default Audio Input", contextRate: 44_100 });
    expect(mic.requests).toEqual([{ audio: MIC_CONSTRAINTS, video: false }]);
    expect(browser.sources[0]?.connected).toEqual([browser.nodes[1]]);
    expect(host.microphoneLive).toBe(true);

    await host.startMicrophone();
    expect(mic.requests).toHaveLength(1);
  });

  test("a refused permission is reported with its reason, and nothing is connected", async () => {
    const denied = Object.assign(new Error("Permission denied"), { name: "NotAllowedError" });
    const browser = fakeBrowser(async () => {
      throw denied;
    });
    const host = await attachedHost(browser.deps);
    expect(await host.startMicrophone()).toEqual({
      ok: false,
      reason: "permission-denied",
      message: "Permission denied",
    });
    expect(browser.sources).toHaveLength(0);
    expect(host.microphoneLive).toBe(false);
  });

  test("a browser without getUserMedia, or a host never attached, is reported too", async () => {
    const without = await attachedHost(fakeBrowser(null).deps);
    expect(await without.startMicrophone()).toMatchObject({ ok: false, reason: "unsupported" });

    const unattached = await AudioHost.open("w.js", fakeBrowser(granted().getUserMedia).deps);
    expect(await unattached.startMicrophone()).toMatchObject({ ok: false, reason: "not-attached" });
  });

  test("maps every Media Capture rejection to a reason the audio card can show", () => {
    const named = (name: string) => Object.assign(new Error(name), { name });
    expect(refusalOf(named("NotAllowedError")).reason).toBe("permission-denied");
    expect(refusalOf(named("SecurityError")).reason).toBe("insecure-context");
    expect(refusalOf(named("NotFoundError")).reason).toBe("no-device");
    expect(refusalOf(named("OverconstrainedError")).reason).toBe("no-device");
    expect(refusalOf(named("NotReadableError")).reason).toBe("device-busy");
    expect(refusalOf(named("AbortError")).reason).toBe("failed");
    expect(refusalOf("weird").reason).toBe("failed");
  });

  test("stopping disconnects the source and stops the track", async () => {
    const mic = granted();
    const browser = fakeBrowser(mic.getUserMedia);
    const host = await attachedHost(browser.deps);
    await host.startMicrophone();
    host.stopMicrophone();
    expect(browser.sources[0]?.disconnected).toBe(true);
    expect(mic.track.stopped).toBe(true);
    expect(host.microphoneLive).toBe(false);
  });

  test("a track the browser ends on its own releases the mic and tells the page", async () => {
    const mic = granted();
    const browser = fakeBrowser(mic.getUserMedia);
    let ended = 0;
    const host = await attachedHost(browser.deps, {}, () => {
      ended += 1;
    });
    await host.startMicrophone();
    mic.track.end();
    expect(ended).toBe(1);
    expect(host.microphoneLive).toBe(false);
    expect(browser.sources[0]?.disconnected).toBe(true);
  });
});
