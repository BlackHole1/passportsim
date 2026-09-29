// The page mounted in advanced mode, where every control exists, driven through DOM events into
// `CommandClient` and `UiJournal`. `simple.test.ts` covers simple mode.

import { describe, expect, test } from "bun:test";
import { EventKind } from "../worker/layout";
import { createPage, sessionStart, type Page } from "./page";
import { installDom, settle, settleUntil } from "./view/testDom";

const window = installDom();
const document = window.document as unknown as Document;

interface Sent {
  readonly cmd: string;
  readonly args: Record<string, unknown>;
}

type Reply = (call: Sent) => { json?: unknown } | { error: { code: string; message: string } };

interface Harness {
  readonly page: Page;
  readonly mount: HTMLElement;
  readonly sent: Sent[];
  readonly toWorker: unknown[];
  advance(ms: number): void;
  setConfirm(answer: boolean): void;
  calls(cmd: string): Sent[];
}

function mountPage(
  reply: Reply = () => ({ json: {} }),
  frames?: Array<() => void>,
  timers?: Array<() => void>,
): Harness {
  const sent: Sent[] = [];
  const toWorker: unknown[] = [];
  const mount = document.createElement("div");
  document.body.appendChild(mount);
  let clock = 0;
  let confirmAnswer = true;

  const page = createPage({
    mount,
    transport: (request) => {
      const call = JSON.parse(request) as Sent;
      sent.push(call);
      const answer = reply(call);
      if ("error" in answer) {
        return Promise.resolve({ err: JSON.stringify(answer.error) });
      }
      return Promise.resolve({ ok: JSON.stringify({ json: answer.json ?? {}, text: "" }) });
    },
    toWorker: (message) => toWorker.push(message),
    rewindSource: { snapshot: () => new Uint8Array(8), frame: () => null },
    now: () => clock,
    confirm: () => confirmAnswer,
    viewport: () => ({ width: 1280, height: 800 }),
    devicePixelRatio: () => 1,
    scheduleFrame: frames ? (callback) => frames.push(callback) : undefined,
    schedule: timers ? (callback) => timers.push(callback) : undefined,
    search: "?mode=advanced",
  });

  return {
    page,
    mount,
    sent,
    toWorker,
    advance: (ms) => {
      clock += ms;
    },
    setConfirm: (answer) => {
      confirmAnswer = answer;
    },
    calls: (cmd) => sent.filter((call) => call.cmd === cmd),
  };
}

function fire(node: Element, type: string): void {
  node.dispatchEvent(new window.Event(type, { bubbles: true }) as unknown as Event);
}

function point(node: Element, type: string, init: Record<string, unknown> = {}): void {
  node.dispatchEvent(new window.PointerEvent(type, { bubbles: true, button: 0, pointerId: 1, ...init }) as unknown as Event);
}

function keyEvent(key: string, init: { repeat?: boolean; target?: unknown; ctrlKey?: boolean } = {}): KeyboardEvent {
  return { key, repeat: init.repeat ?? false, ctrlKey: init.ctrlKey ?? false, metaKey: false, altKey: false, target: init.target ?? null } as unknown as KeyboardEvent;
}

/** A pacing report at virtual time `ms`, which is what lets a waiting release go out. */
function runTo(app: Harness, ms: number): void {
  app.page.stats({ mode: "Wall", nowPs: BigInt(ms) * 1_000_000_000n, realTimeFactor: 1, reanchors: 0, sliceVtPs: 0n });
}

function buttonNamed(root: ParentNode, label: string): HTMLButtonElement {
  const found = [...root.querySelectorAll("button")].find(
    (node) => node.textContent?.trim() === label,
  );
  if (!found) {
    throw new Error(`no button labelled ${label}`);
  }
  return found as HTMLButtonElement;
}

function query(root: ParentNode, selector: string): HTMLElement {
  const node = root.querySelector(selector);
  if (!node) {
    throw new Error(`no element matching ${selector}`);
  }
  return node as HTMLElement;
}

describe("a press on the skin", () => {
  test("down presses at once and up releases once the guest has held it 80 ms", async () => {
    const app = mountPage();
    const ok = query(app.mount, '[data-control="ok"]');

    point(ok, "pointerdown");
    await settle();
    expect(app.sent).toEqual([{ cmd: "input", args: { button: "ok", action: "press" } }]);
    expect(app.page.journal.last()?.args).toEqual({ button: "ok", action: "press" });

    // A quick click: let go before any guest time passed, so the release waits for it.
    point(ok, "pointerup");
    await settle();
    expect(app.calls("input")).toHaveLength(1);
    runTo(app, 79);
    await settle();
    expect(app.calls("input")).toHaveLength(1);
    runTo(app, 80);
    await settle();
    expect(app.sent).toEqual([
      { cmd: "input", args: { button: "ok", action: "press" } },
      { cmd: "input", args: { button: "ok", action: "release" } },
    ]);
    expect(app.page.journal.list().map((entry) => entry.args)).toEqual([
      { button: "ok", action: "press" },
      { button: "ok", action: "release" },
    ]);
  });

  test("a hold keeps the button down until it is let go, however long", async () => {
    const app = mountPage();
    const down = query(app.mount, '[data-control="down"]');
    point(down, "pointerdown");
    await settle();
    runTo(app, 5_000);
    await settle();
    expect(app.calls("input").map((call) => call.args.action)).toEqual(["press"]);
    point(down, "pointerup");
    await settle();
    expect(app.calls("input").map((call) => call.args.action)).toEqual(["press", "release"]);
  });

  test("the power button is pressed and released like the others", async () => {
    const app = mountPage();
    const power = query(app.mount, '[data-control="power"]');
    point(power, "pointerdown");
    await settle();
    runTo(app, 700);
    point(power, "pointerup");
    await settle();
    expect(app.calls("input").map((call) => call.args)).toEqual([
      { button: "power", action: "press" },
      { button: "power", action: "release" },
    ]);
  });

  test("a cancelled pointer, a lost capture or leaving the button releases it", async () => {
    for (const end of ["pointercancel", "lostpointercapture", "pointerout"]) {
      const app = mountPage();
      const up = query(app.mount, '[data-control="up"]');
      point(up, "pointerdown");
      await settle();
      runTo(app, 100);
      await settle();
      point(up, end, end === "pointerout" ? { relatedTarget: document.body } : {});
      await settle();
      expect({ end, actions: app.calls("input").map((call) => call.args.action) }).toEqual({ end, actions: ["press", "release"] });
    }
  });

  test("only the main button presses: a right click opens nothing and sends nothing", async () => {
    const app = mountPage();
    const ok = query(app.mount, '[data-control="ok"]');
    point(ok, "pointerdown", { button: 2 });
    await settle();
    expect(app.calls("input")).toEqual([]);
  });

  test("a refused press leaves nothing to release", async () => {
    const app = mountPage((call) => (call.cmd === "input" ? { error: { code: "E_STATE", message: "no machine" } } : { json: {} }));
    const ok = query(app.mount, '[data-control="ok"]');
    point(ok, "pointerdown");
    await settle();
    point(ok, "pointerup");
    runTo(app, 1_000);
    await settle();
    expect(app.calls("input").map((call) => call.args.action)).toEqual(["press"]);
  });

  test("the press instant comes from the machine's answer, not the page's last report", async () => {
    const app = mountPage((call) => (call.cmd === "input" ? { json: { vt_us: 500_000, input: { press_vt_us: 500_000 } } } : { json: {} }));
    const ok = query(app.mount, '[data-control="ok"]');
    point(ok, "pointerdown");
    await settle();
    point(ok, "pointerup");
    runTo(app, 579);
    await settle();
    expect(app.calls("input")).toHaveLength(1);
    runTo(app, 580);
    await settle();
    expect(app.calls("input").map((call) => call.args.action)).toEqual(["press", "release"]);
  });
});

describe("a press on the keyboard", () => {
  test("keydown presses, an auto-repeat is swallowed, keyup releases", async () => {
    const app = mountPage();
    expect(app.page.key(keyEvent("ArrowDown"), true)).toBe(true);
    expect(app.page.key(keyEvent("ArrowDown", { repeat: true }), true)).toBe(true);
    expect(app.page.key(keyEvent("ArrowDown", { repeat: true }), true)).toBe(true);
    await settle();
    runTo(app, 300);
    await settle();
    expect(app.calls("input").map((call) => call.args)).toEqual([{ button: "down", action: "press" }]);
    expect(app.page.key(keyEvent("ArrowDown"), false)).toBe(true);
    await settle();
    expect(app.calls("input").map((call) => call.args)).toEqual([
      { button: "down", action: "press" },
      { button: "down", action: "release" },
    ]);
  });

  test("the key comes up whatever took the focus or a modifier in between", async () => {
    const app = mountPage();
    app.page.key(keyEvent("Enter"), true);
    await settle();
    runTo(app, 300);
    await settle();
    const field = document.createElement("input");
    expect(app.page.key(keyEvent("Enter", { target: field, ctrlKey: true }), false)).toBe(true);
    await settle();
    expect(app.calls("input").map((call) => call.args.action)).toEqual(["press", "release"]);
    // A keyup of a key that pressed nothing is not the device's.
    expect(app.page.key(keyEvent("ArrowUp"), false)).toBe(false);
  });

  test("losing the page's focus lets every held control go", async () => {
    const app = mountPage();
    app.page.key(keyEvent("ArrowUp"), true);
    point(query(app.mount, '[data-control="power"]'), "pointerdown");
    await settle();
    app.page.releaseControls();
    runTo(app, 80);
    await settle();
    expect(app.calls("input").map((call) => call.args)).toEqual([
      { button: "up", action: "press" },
      { button: "power", action: "press" },
      { button: "up", action: "release" },
      { button: "power", action: "release" },
    ]);
  });
});

describe("the transport buttons", () => {
  test("Run and Pause call `clock --op resume|pause`, so they are in the journal", async () => {
    const app = mountPage();

    buttonNamed(app.mount, "Run").click();
    await settle();
    buttonNamed(app.mount, "Pause").click();
    await settle();

    // A machine is built `deterministic`, where a resume is refused, so the first Run switches the
    // clock to `realtime` first, journaled like the rest.
    expect(app.calls("clock")).toEqual([
      { cmd: "clock", args: { op: "set_mode", mode: "realtime" } },
      { cmd: "clock", args: { op: "resume" } },
      { cmd: "clock", args: { op: "pause" } },
    ]);
    expect(app.page.journal.list().map((entry) => entry.command)).toEqual(["clock", "clock", "clock"]);
  });

  test("the clock is switched once per machine, and again for the one a loaded image replaces it with", async () => {
    const app = mountPage();
    buttonNamed(app.mount, "Run").click();
    await settle();
    buttonNamed(app.mount, "Pause").click();
    await settle();
    buttonNamed(app.mount, "Run").click();
    await settle();

    expect(app.calls("clock").filter((call) => call.args.op === "set_mode")).toHaveLength(1);
  });

  test("a refused clock switch leaves the Worker's pacing alone", async () => {
    const app = mountPage((call) =>
      call.args.op === "set_mode" ? { error: { code: "E_LEASE", message: "an agent holds the clock" } } : { json: {} },
    );
    buttonNamed(app.mount, "Run").click();
    await settle();

    expect(app.toWorker).toEqual([]);
    expect(app.calls("clock")).toEqual([{ cmd: "clock", args: { op: "set_mode", mode: "realtime" } }]);
  });

  test("the Worker's mode follows the accepted command rather than preceding it", async () => {
    const app = mountPage();
    buttonNamed(app.mount, "Run").click();
    await settle();
    expect(app.toWorker).toEqual([{ type: "mode", mode: { kind: "Wall", rate: 1 } }]);
  });

  test("a refused pace leaves the Worker where it was", async () => {
    const app = mountPage((call) =>
      call.cmd === "clock"
        ? { error: { code: "E_LEASE", message: "an agent holds the clock" } }
        : { json: {} },
    );

    buttonNamed(app.mount, "Run").click();
    await settle();

    // The refusal is journaled, because it is a thing the user did; the machine is not re-paced.
    expect(app.page.journal.last()?.outcome.state).toBe("failed");
    expect(app.toWorker).toEqual([]);
  });
});

describe("the USB connector strip", () => {
  test("clicking the state the machine is already in sends nothing", async () => {
    const app = mountPage();
    query(app.mount, '[data-usb="U3"]').click();
    await settle();
    expect(app.calls("input")).toEqual([]);
  });

  test("it sends only what changed, and the card agrees with it afterwards", async () => {
    const app = mountPage();

    query(app.mount, '[data-usb="U2"]').click();
    await settle();
    expect(app.calls("input")).toEqual([
      { cmd: "input", args: { button: "usb", action: "close" } },
    ]);

    // The card and the strip share one state: back to U3 is one call, not a cable plus a client.
    query(app.mount, '[data-usb-card="U3"]').click();
    await settle();
    expect(app.calls("input")).toHaveLength(2);
    expect(app.calls("input")[1]).toEqual({
      cmd: "input",
      args: { button: "usb", action: "open" },
    });
  });

  test("U1 is offered nowhere, because the rail is the power button's, and says why", async () => {
    const app = mountPage();
    for (const selector of ['[data-usb="U1"]', '[data-usb-card="U1"]']) {
      const button = query(app.mount, selector) as HTMLButtonElement;
      expect(button.getAttribute("aria-disabled")).toBe("true");
      // Its explanation stays reachable, which a `disabled` button's tooltip is not in every engine.
      expect(button.title).toContain("POWER");
      button.click();
    }
    await settle();
    expect(app.calls("input")).toEqual([]);
    expect(query(app.mount, '[data-usb-card="U2"]').textContent).toBe("Computer, port closedU2");
    expect(query(app.mount, '[data-usb="U0"]').textContent).toBe("UnpluggedU0");
  });
});

describe("the NFC card", () => {
  test("Lock sends `lock` alone, so a confirmed lock cannot also erase the tag", async () => {
    const app = mountPage();
    buttonNamed(app.mount, "Lock tag").click();
    await settle();
    expect(app.calls("nfc_tag")).toEqual([{ cmd: "nfc_tag", args: { lock: true } }]);
  });

  test("the editor puts records on the tag and takes them off again", async () => {
    const app = mountPage();

    query(app.mount, '[data-action="ndef-add"]').click();
    await settleUntil(() => app.mount.querySelectorAll(".ndef-list li").length === 1);
    expect(app.calls("nfc_tag")[0]?.args).toEqual({
      ndef: [{ type: "uri", uri: "https://example.com/p" }],
    });

    const list = query(app.mount, ".ndef-list");
    expect(list.querySelectorAll("li")).toHaveLength(1);

    buttonNamed(list, "remove").click();
    await settleUntil(() => app.calls("nfc_tag").length === 2);
    expect(app.calls("nfc_tag")[1]?.args).toEqual({ ndef: [] });
  });

  test("a refused write leaves the card showing the tag the machine still has", async () => {
    const app = mountPage((call) =>
      call.cmd === "nfc_tag"
        ? { error: { code: "E_HOST_UNSUPPORTED", message: "the nfc caps group is off" } }
        : { json: {} },
    );
    query(app.mount, '[data-action="ndef-add"]').click();
    await settle();
    expect(app.mount.querySelectorAll(".ndef-list li")).toHaveLength(0);
  });

  test("once locked, the editor stops offering writes the tag can no longer take", async () => {
    const app = mountPage();
    query(app.mount, '[data-action="ndef-add"]').click();
    await settleUntil(() => app.mount.querySelectorAll(".ndef-list li").length === 1);
    buttonNamed(app.mount, "Lock tag").click();
    await settleUntil(() => (query(app.mount, '[data-action="ndef-add"]') as HTMLButtonElement).disabled);

    expect((query(app.mount, '[data-action="ndef-add"]') as HTMLButtonElement).disabled).toBe(true);
    expect(buttonNamed(query(app.mount, ".ndef-list"), "remove").disabled).toBe(true);
    expect(query(app.mount, '[data-card="nfc"]').textContent).toContain("the OTP bits are set");
    // And the record the user wrote is still on the tag: the lock did not erase it.
    expect(app.mount.querySelectorAll(".ndef-list li")).toHaveLength(1);
  });

  test("a declined confirmation sends nothing at all", async () => {
    const app = mountPage();
    app.setConfirm(false);
    buttonNamed(app.mount, "Lock tag").click();
    await settle();
    expect(app.calls("nfc_tag")).toEqual([]);
  });
});

describe("the Wi-Fi card", () => {
  // The toggle stays disabled until the card has a form for the guest and host ports.
  test("the bridge toggle is disabled while the card has no route to bridge", () => {
    const app = mountPage();
    const bridge = query(app.mount, '#f-wifi-bridge') as HTMLInputElement;
    expect(bridge.disabled).toBe(true);
    expect(bridge.checked).toBe(false);
  });

  test("the live-bridge warning is a note, not the error slot a later call would clear", async () => {
    const app = mountPage();
    const card = query(app.mount, '[data-card="wifi"]');
    expect(card.textContent).toContain("live, not replayable");
    // A successful action clears the error slot; the warning has to survive it.
    buttonNamed(card, "Save pcap").click();
    await settle();
    expect(card.textContent).toContain("live, not replayable");
  });
});

describe("accessible names", () => {
  test("no two controls in the page share a DOM id", () => {
    const app = mountPage();
    const ids = [...app.mount.querySelectorAll("[id]")].map((node) => node.id);
    const seen = new Set(ids);
    expect([...seen].sort()).toEqual([...ids].sort());
  });

  test("a shared label does not make two cards point at one input", () => {
    const app = mountPage();
    const labels = [...app.mount.querySelectorAll("label")].filter(
      (node) => node.textContent === "SSID",
    );
    expect(labels).toHaveLength(2);
    const targets = labels.map((node) => node.getAttribute("for"));
    expect(new Set(targets).size).toBe(2);
    for (const target of targets) {
      expect(app.mount.querySelectorAll(`#${target}`)).toHaveLength(1);
    }
  });
});

describe("the rewind scrubber", () => {
  async function withPoints(): Promise<Harness & { scrubber: HTMLInputElement }> {
    const app = mountPage((call) =>
      call.cmd === "snapshot" && call.args.op === "save"
        ? { json: { op: "save", name: "rewind-2-4.000s" } }
        : { json: {} },
    );
    app.page.stats({
      mode: "Wall",
      nowPs: 4_000_000_000_000n,
      realTimeFactor: 1,
      reanchors: 0,
      sliceVtPs: 0n,
    });
    query(app.mount, '[data-action="snapshot-save"]').click();
    await settle();
    return { ...app, scrubber: query(app.mount, '[aria-label="Rewind to an earlier point"]') as HTMLInputElement };
  }

  test("a manual save is stamped with virtual time, not zero", async () => {
    const app = await withPoints();
    const saved = app.page.rewind.list()[1];
    expect(saved?.vtPs).toBe(4_000_000_000_000n);
    expect(app.calls("snapshot")[0]?.args).toEqual({ op: "save", name: "rewind-2-4.000s" });
    expect(app.page.rewind.stats().spanPs).toBe(0n);
  });

  test("it restores by the name the registry saved under, never by the ring's own seq", async () => {
    const app = await withPoints();
    app.scrubber.value = "2";
    fire(app.scrubber, "change");
    await settle();
    expect(app.calls("snapshot")[1]).toEqual({
      cmd: "snapshot",
      args: { op: "restore", name: "rewind-2-4.000s" },
    });
  });

  test("a point the registry never saved says so instead of sending its seq", async () => {
    const app = await withPoints();
    app.scrubber.value = "1";
    fire(app.scrubber, "change");
    await settle();
    expect(app.calls("snapshot")).toHaveLength(1);
    expect(query(app.mount, '[data-card="snapshots"] .card-error').textContent).toContain(
      "no registry snapshot",
    );
  });

  test("a drag restores once, on release, not once per pixel", async () => {
    const app = await withPoints();
    for (const value of ["1", "2", "1", "2"]) {
      app.scrubber.value = value;
      fire(app.scrubber, "input");
    }
    await settle();
    // `input` only moves the label and the overlay frame; nothing has been restored yet.
    expect(app.calls("snapshot")).toHaveLength(1);

    fire(app.scrubber, "change");
    await settle();
    expect(app.calls("snapshot").filter((call) => call.args.op === "restore")).toHaveLength(1);
  });
});

describe("the Events tab", () => {
  test("it lists what the machine reported, decoded through the generated EventKind", async () => {
    const app = mountPage();
    app.page.events([
      { kind: EventKind.Reset, vtPs: 0n, arg: 0n },
      { kind: EventKind.Panic, vtPs: 1_500_000_000_000n, arg: 3n },
    ]);
    await settle();
    const rows = [...query(app.mount, "#pane-events").querySelectorAll("tbody tr")];
    expect(rows.map((row) => row.querySelectorAll("td")[2]?.textContent)).toEqual([
      "Reset",
      "Panic",
    ]);
    expect(rows[1]?.querySelectorAll("td")[0]?.textContent).toBe("1.500 s");
  });

  test("an input is an event too, taken from the journal the control writes", async () => {
    const app = mountPage();
    const ok = query(app.mount, '[data-control="ok"]');
    point(ok, "pointerdown");
    await settle();

    const rows = [...query(app.mount, "#pane-events").querySelectorAll("tbody tr")];
    expect(rows).toHaveLength(1);
    expect(rows[0]?.getAttribute("data-source")).toBe("ui");
    expect(rows[0]?.querySelectorAll("td")[2]?.textContent).toBe("input");
  });

  test("a kind this bundle does not know is shown, not dropped", async () => {
    const app = mountPage();
    app.page.events([{ kind: 99, vtPs: 0n, arg: 0n }]);
    await settle();
    expect(query(app.mount, "#pane-events").textContent).toContain("kind 99");
  });
});

describe("the UI tree tab", () => {
  const MENU = [
    "- obj [0,0 240x320] bg=#1689e8 e1",
    "  - obj [11,52 102x40] bg=#ffd928 border=#ffffff e28",
    '    - label "Display" [36,64 53x16] e29',
  ].join("\n");
  let rev = 0;
  const reply: Reply = (call) => {
    if (call.cmd !== "ui") {
      return { json: {} };
    }
    rev += 1;
    return {
      json: { ui_rev: rev, text: MENU, diff: [], counts: { objects: 63, shown: 3 }, screen: { w: 240, h: 320 } },
    };
  };
  const frame = (app: Harness) => app.page.events([{ kind: EventKind.Frame, vtPs: 0n, arg: 1n }]);

  test("it reads nothing while hidden, reads on opening, and follows frames with ui --diff", async () => {
    const timers: Array<() => void> = [];
    const app = mountPage(reply, undefined, timers);
    frame(app);
    await settle();
    expect(app.calls("ui")).toEqual([]);
    expect(timers).toEqual([]);

    fire(query(app.mount, "#tab-ui-tree"), "click");
    await settle();
    expect(app.calls("ui").map((call) => call.args)).toEqual([{ include_style: true }]);
    const pane = query(app.mount, "#pane-ui-tree");
    expect(pane.hidden).toBe(false);
    expect(query(pane, '[data-ref="e29"]').textContent).toBe('- label "Display" [36,64 53x16] e29');

    // A frame inside the interval waits for its timer; the timer's read names the revision held.
    frame(app);
    expect(app.calls("ui")).toHaveLength(1);
    app.advance(1_000);
    timers.splice(0).forEach((run) => run());
    await settle();
    const second = app.calls("ui")[1]?.args;
    expect(second).toEqual({ diff: rev - 1, include_style: true });

    // Hidden again: frames read nothing.
    fire(query(app.mount, "#tab-console"), "click");
    app.advance(5_000);
    frame(app);
    timers.splice(0).forEach((run) => run());
    await settle();
    expect(app.calls("ui")).toHaveLength(2);
  });

  test("hovering a row outlines that object's box over the glass, at the glass's scale", async () => {
    const app = mountPage(reply);
    fire(query(app.mount, "#tab-ui-tree"), "click");
    await settle();
    const card = query(app.mount, '#pane-ui-tree [data-ref="e28"]');
    expect(app.mount.querySelector(".skin-screen .ui-highlight")).toBeNull();
    // React derives enter and leave from `mouseover` and `mouseout`, which is what a browser sends.
    card.dispatchEvent(new window.MouseEvent("mouseover", { bubbles: true }) as unknown as Event);
    await settle();
    const glass = query(app.mount, "canvas.glass");
    const scale = Number.parseFloat(glass.style.width) / 240;
    expect(scale).toBeGreaterThan(0);
    const box = query(app.mount, ".skin-screen .ui-highlight");
    expect(Number.parseFloat(box.style.left)).toBeCloseTo(11 * scale, 6);
    expect(Number.parseFloat(box.style.top)).toBeCloseTo(52 * scale, 6);
    expect(Number.parseFloat(box.style.width)).toBeCloseTo(102 * scale, 6);
    expect(Number.parseFloat(box.style.height)).toBeCloseTo(40 * scale, 6);
    card.dispatchEvent(new window.MouseEvent("mouseout", { bubbles: true }) as unknown as Event);
    await settle();
    expect(app.mount.querySelector(".skin-screen .ui-highlight")).toBeNull();
  });
});

describe("Copy as CLI, Copy as scenario step and Record scenario", () => {
  async function clickOk(app: Harness): Promise<void> {
    const ok = query(app.mount, '[data-control="ok"]');
    point(ok, "pointerdown");
    point(ok, "pointerup");
    await settle();
    runTo(app, 80);
    await settle();
  }

  test("the rows of a click copy the exact CLI lines and the exact steps", async () => {
    const app = mountPage();
    await clickOk(app);
    const pane = query(app.mount, "#pane-events");
    const [row, released] = [...pane.querySelectorAll('tbody tr[data-source="ui"]')] as HTMLElement[];
    fire(query(row!, '[data-copy="cli"]'), "click");
    await settleUntil(() => query(pane, '[data-export="status"]').textContent !== "");
    const output = query(pane, '[data-export="output"]') as HTMLTextAreaElement;
    expect(output.value).toBe("passportsim input ok press");
    expect(query(pane, '[data-export="status"]').textContent).toContain("no clipboard");

    fire(query(row!, '[data-copy="step"]'), "click");
    await settle();
    expect(output.value).toBe("- press: {button: ok, action: press}");
    fire(query(released!, '[data-copy="cli"]'), "click");
    await settle();
    expect(output.value).toBe("passportsim input ok release");
  });

  test("a machine row has no copy buttons: it is not something the page asked for", async () => {
    const app = mountPage();
    app.page.events([{ kind: EventKind.Reset, vtPs: 0n, arg: 0n }]);
    await settle();
    const row = query(query(app.mount, "#pane-events"), 'tbody tr[data-source="machine"]');
    expect(row.querySelectorAll("[data-copy]")).toHaveLength(0);
  });

  test("Record scenario exports the session from the world it started in", async () => {
    const app = mountPage();
    const pane = query(app.mount, "#pane-events");
    const record = query(pane, '[data-export="record"]');
    fire(record, "click");
    expect(app.page.recorder.recording).toBe(true);
    await clickOk(app);
    fire(record, "click");
    await settle();
    const yaml = (query(pane, '[data-export="output"]') as HTMLTextAreaElement).value;
    expect(yaml).toContain("schema: passportsim/scenario@1\nname: recorded-session\n");
    expect(yaml).toContain("image: official\nsetup: {power: on, usb: open}\nsteps:\n");
    expect(yaml).toContain("    press: {button: ok, action: press}\n  - delay: 80ms\n");
    expect(yaml).toContain("    press: {button: ok, action: release}\n");
    expect(app.page.recorder.recording).toBe(false);
  });

  test("the setup follows the USB card's U-state", () => {
    expect(sessionStart("official", "U0")).toEqual({ image: "official", power: "on", usb: "unplugged" });
    expect(sessionStart("official", "U1")).toEqual({ image: "official", power: "off", usb: "charger" });
    expect(sessionStart(null, "U2")).toEqual({ image: null, power: "on", usb: "host" });
  });
});

describe("the renderer line", () => {
  const line = (mount: HTMLElement) => mount.querySelector(".skin-display") as HTMLElement;

  test("each state the Worker's display message can carry is visible on the page", async () => {
    const { page, mount } = mountPage();
    expect(line(mount).getAttribute("role")).toBe("status");

    page.display({ backend: "webgl1", reason: null, contextLost: false });
    await settle();
    expect(line(mount).textContent).toBe("renderer WebGL1");
    expect(line(mount).dataset.level).toBe("ok");

    page.display({ backend: "webgl1", reason: "no highp fragment floats; colours may be off by one", contextLost: false });
    await settle();
    expect(line(mount).textContent).toBe("renderer WebGL1: no highp fragment floats; colours may be off by one");
    expect(line(mount).dataset.level).toBe("inexact");

    page.display({ backend: "canvas2d", reason: "WebGL1 is unavailable", contextLost: false });
    await settle();
    expect(line(mount).textContent).toBe("renderer 2D fallback: WebGL1 is unavailable");
    expect(line(mount).dataset.level).toBe("fallback");

    page.display({ backend: "webgl1", reason: null, contextLost: true });
    await settle();
    expect(line(mount).textContent).toContain("WebGL context lost");
    expect(line(mount).dataset.level).toBe("lost");

    page.display({ backend: "none", reason: "the page transferred no OffscreenCanvas to the Worker", contextLost: false });
    await settle();
    expect(line(mount).textContent).toBe("no renderer: the page transferred no OffscreenCanvas to the Worker");
    expect(line(mount).dataset.level).toBe("none");

    // A restore after a loss goes back to naming the renderer.
    page.display({ backend: "webgl1", reason: null, contextLost: false });
    await settle();
    expect(line(mount).textContent).toBe("renderer WebGL1");
  });
});

describe("the status line under the skin", () => {
  const line = (mount: HTMLElement) => mount.querySelector(".skin-status") as HTMLElement;
  const menu = {
    backlight: 1023,
    backlightScale: 1024,
    powered: true,
    sleeping: false,
    displayOn: true,
    inverted: true,
    glassComplement: false,
  };

  test("states nothing about a panel no machine has reported", () => {
    const { mount } = mountPage();
    expect(line(mount).textContent).toBe("backlight ? | panel ? | U3");
  });

  test("reads the panel state the Worker reports, and follows it", async () => {
    const { page, mount } = mountPage();
    page.panel(menu);
    await settle();
    expect(line(mount).textContent).toBe("backlight 100% | panel on | U3");

    page.panel({ ...menu, backlight: 512 });
    await settle();
    expect(line(mount).textContent).toBe("backlight 50% | panel on | U3");
    page.panel({ ...menu, displayOn: false });
    await settle();
    expect(line(mount).textContent).toBe("backlight 100% | panel display off | U3");
    page.panel({ ...menu, sleeping: true });
    await settle();
    expect(line(mount).textContent).toBe("backlight 100% | panel sleeping | U3");
    page.panel({ ...menu, powered: false, backlight: 0 });
    await settle();
    expect(line(mount).textContent).toBe("backlight 0% | panel off | U3");
    page.panel({ ...menu, backlightScale: 0 });
    await settle();
    expect(line(mount).textContent).toBe("backlight ? | panel on | U3");
  });

  test("says inverted only when the glass shows the complement, not for INVON", async () => {
    const { page, mount } = mountPage();
    page.panel({ ...menu, inverted: true, glassComplement: false });
    await settle();
    expect(line(mount).textContent).toBe("backlight 100% | panel on | U3");
    page.panel({ ...menu, inverted: false, glassComplement: true });
    await settle();
    expect(line(mount).textContent).toBe("backlight 100% | panel inverted | U3");
  });
});

describe("the image loader", () => {
  function mergedBin(): Uint8Array {
    const bytes = new Uint8Array(0x20_000);
    bytes[0] = 0xe9;
    return bytes;
  }

  const headerImage = (mount: HTMLElement) => query(mount, '[data-field="image"]').textContent;

  test("a loaded image pauses the machine through the registry, reboots the Worker on its assets and names it", async () => {
    const app = mountPage();
    expect(headerImage(app.mount)).toBe("official");

    const offered = app.page.loader.offer({
      root: "pk",
      files: [
        { path: "FoloToy-AI-Passport-8MB.bin", size: 0x20_000, read: () => Promise.resolve(mergedBin()) },
        { path: "FoloToy-AI-Passport.elf", size: 4, read: () => Promise.resolve(new Uint8Array([0x7f, 0x45, 0x4c, 0x46])) },
      ],
    });
    await settle();

    // The load is not done until the Worker says the machine is up, so the strip does not claim
    // the image is running before it is.
    expect(query(app.mount, "[data-loader]").getAttribute("data-loader-state")).toBe("loading");
    app.page.ready();
    await offered;
    await settle();
    expect(query(app.mount, "[data-loader]").getAttribute("data-loader-state")).toBe("loaded");

    // Every control calls a registry command: the pause is journaled, and the new machine is switched
    // to realtime and resumed as the Run button would.
    expect(app.calls("clock")).toEqual([
      { cmd: "clock", args: { op: "pause" } },
      { cmd: "clock", args: { op: "set_mode", mode: "realtime" } },
      { cmd: "clock", args: { op: "resume" } },
    ]);
    expect(app.toWorker[0]).toEqual({ type: "mode", mode: { kind: "Paused" } });
    const boot = app.toWorker[1] as { type: string; config: string; assets: { kind: number }[] };
    expect(boot.type).toBe("boot");
    expect(JSON.parse(boot.config)).toEqual({ fw: "pk" });
    expect(boot.assets.map((asset) => asset.kind)).toEqual([1, 2]);
    expect(headerImage(app.mount)).toBe("pk");
  });

  test("a refused drop changes nothing: no reboot, no pause, and the header keeps the demo", async () => {
    const app = mountPage();
    await app.page.loader.offer({
      root: null,
      files: [{ path: "notes.txt", size: 2, read: () => Promise.resolve(new Uint8Array([1, 2])) }],
    });
    await settle();

    expect(app.toWorker).toEqual([]);
    expect(app.sent).toEqual([]);
    expect(headerImage(app.mount)).toBe("official");
    expect(query(app.mount, "[data-loader]").getAttribute("data-loader-state")).toBe("refused");
  });

  test("`ready` runs what was booted, with the resume the Run button sends", async () => {
    const app = mountPage();
    app.page.ready();
    await settle();

    expect(app.calls("clock")).toEqual([
      { cmd: "clock", args: { op: "set_mode", mode: "realtime" } },
      { cmd: "clock", args: { op: "resume" } },
    ]);
    expect(app.toWorker).toEqual([{ type: "mode", mode: { kind: "Wall", rate: 1 } }]);
  });

  test("the machine a load brings up is switched to realtime on its own, not on the last one's word", async () => {
    const app = mountPage();
    // The demo's own resume: this machine's clock is now realtime.
    app.page.ready();
    await settle();
    const offered = app.page.loader.offer({
      root: "pk",
      files: [{ path: "a.bin", size: 0x20_000, read: () => Promise.resolve(mergedBin()) }],
    });
    await settle();
    app.page.ready();
    await offered;
    await settle();

    // One switch per machine. With the flag kept per page, the second machine would be recorded as
    // realtime although built deterministic, and its resume would be refused.
    expect(app.calls("clock").filter((call) => call.args.op === "set_mode")).toHaveLength(2);
    expect(app.toWorker.at(-1)).toEqual({ type: "mode", mode: { kind: "Wall", rate: 1 } });
  });

  test("a Worker error is stated on the loader strip rather than only in the console", async () => {
    const app = mountPage();
    app.page.workerError("E_ASSET_MISSING: the firmware bundle for `official` is not served (404)");
    await settle();
    const strip = query(app.mount, "[data-loader]");
    expect(strip.getAttribute("data-loader-state")).toBe("error");
    expect(strip.textContent).toContain("404");
  });

  test("the answer to a boot the load replaced is not the load's: neither its refusal nor its `ready`", async () => {
    const app = mountPage();
    const offered = app.page.loader.offer({
      root: null,
      files: [{ path: "a.bin", size: 0x20_000, read: () => Promise.resolve(mergedBin()) }],
    });
    await settle();
    const boot = app.toWorker[1] as { readonly type: string; readonly token?: string };
    expect(boot.type).toBe("boot");
    expect(typeof boot.token).toBe("string");

    // The demo's boot, still in flight, fails while the dropped image comes up. That refusal belongs
    // to the machine this load replaces.
    app.page.workerError("E_ASSET_MISSING: the firmware bundle for `official` is not served (404)", "demo");
    await settle();
    expect(query(app.mount, "[data-loader]").getAttribute("data-loader-state")).toBe("loading");

    // Nor does the demo coming up finish the load, or run a machine the page has already replaced.
    app.page.ready("demo");
    await settle();
    expect(query(app.mount, "[data-loader]").getAttribute("data-loader-state")).toBe("loading");
    expect(app.calls("clock")).toEqual([{ cmd: "clock", args: { op: "pause" } }]);

    app.page.ready(boot.token);
    await offered;
    await settle();
    expect(query(app.mount, "[data-loader]").getAttribute("data-loader-state")).toBe("loaded");
    expect(headerImage(app.mount)).toBe("a");
  });

  test("a boot the Worker refuses ends the load with that message, and the header keeps the old image", async () => {
    const app = mountPage();
    const offered = app.page.loader.offer({
      root: null,
      files: [{ path: "a.bin", size: 0x20_000, read: () => Promise.resolve(mergedBin()) }],
    });
    await settle();
    app.page.workerError("E_ASSET_MISSING: the core refused the assets");
    await settle();
    await offered;
    await settle();

    const strip = query(app.mount, "[data-loader]");
    expect(strip.getAttribute("data-loader-state")).toBe("error");
    expect(strip.textContent).toContain("E_ASSET_MISSING");
    expect(strip.getAttribute("data-loader-image")).toBe("official");
  });

  // Repainting per `stats` message cost most of the page's host CPU at 1x (about 120 layouts a
  // second), so the panel and header repaint once a frame, with the newest numbers.
  test("many stats messages between two frames repaint the header once, with the last value", async () => {
    const frames: Array<() => void> = [];
    const app = mountPage(() => ({ json: {} }), frames);
    const vt = () => query(app.mount, '[data-field="vt"]').textContent;
    const before = vt();

    for (const ms of [1, 2, 3, 4, 5]) {
      app.page.stats({
        mode: "Wall",
        nowPs: BigInt(ms) * 1_000_000_000n,
        realTimeFactor: 1,
        reanchors: 0,
        sliceVtPs: 0n,
      });
    }
    expect(frames.length).toBe(1);
    expect(vt()).toBe(before);

    frames[0]?.();
    await settle();
    expect(vt()).toBe("vt 0.005 s");

    // The next message asks for a frame again, rather than being swallowed by the spent one.
    app.page.stats({
      mode: "Wall",
      nowPs: 6_000_000_000n,
      realTimeFactor: 1,
      reanchors: 0,
      sliceVtPs: 0n,
    });
    expect(frames.length).toBe(2);
  });
});
