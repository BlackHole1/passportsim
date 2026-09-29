import { describe, expect, test } from "bun:test";
import { CommandClient } from "../api/client";
import { UiJournal } from "../api/journal";
import { ButtonHolds, MIN_HOLD_US, edgeArgs, pressedAt } from "./controls";
import * as usb from "./panels/usb";
import { DEVICE_BINDINGS, isTextField, keyAction } from "./keymap";
import { LADDER_BUTTONS } from "./skin";

const PS_PER_US = 1_000_000n;
const MIN_HOLD_PS = MIN_HOLD_US * PS_PER_US;

describe("edges", () => {
  test("down is the CLI's `press` and up its `release`, for every control", () => {
    for (const control of [...LADDER_BUTTONS.map((spec) => spec.id), "power" as const]) {
      expect(edgeArgs(control, true)).toEqual({ button: control, action: "press" });
      expect(edgeArgs(control, false)).toEqual({ button: control, action: "release" });
    }
  });

  test("the minimum hold is the CLI's click length, 80 ms of guest time", () => {
    expect(MIN_HOLD_US).toBe(80_000n);
  });

  test("the press instant is read from the answer, in picoseconds", () => {
    expect(pressedAt({ input: { press_vt_us: 1_500 }, vt_us: 9 })).toBe(1_500n * PS_PER_US);
    expect(pressedAt({ vt_us: 7 })).toBe(7n * PS_PER_US);
    expect(pressedAt({})).toBeNull();
    expect(pressedAt(null)).toBeNull();
    expect(pressedAt({ vt_us: -1 })).toBeNull();
  });
});

describe("ButtonHolds", () => {
  test("a release waits until the press has lasted the minimum hold in guest time", () => {
    const holds = new ButtonHolds();
    expect(holds.press("ok")).toBe(true);
    holds.pressed("ok", 1_000n);
    expect(holds.letGo("ok")).toBe(true);
    expect(holds.waiting).toBe(true);
    expect(holds.due(1_000n)).toEqual([]);
    expect(holds.due(1_000n + MIN_HOLD_PS - 1n)).toEqual([]);
    expect(holds.due(1_000n + MIN_HOLD_PS)).toEqual(["ok"]);
    expect(holds.isDown("ok")).toBe(false);
    expect(holds.waiting).toBe(false);
  });

  test("a control still held is never due, however long it has been down", () => {
    const holds = new ButtonHolds();
    holds.press("down");
    holds.pressed("down", 0n);
    expect(holds.due(10n * MIN_HOLD_PS)).toEqual([]);
    expect(holds.isDown("down")).toBe(true);
  });

  test("a release is not due before the press has answered with its instant", () => {
    const holds = new ButtonHolds();
    holds.press("up");
    holds.letGo("up");
    expect(holds.due(10n * MIN_HOLD_PS)).toEqual([]);
    holds.pressed("up", 0n);
    expect(holds.due(MIN_HOLD_PS)).toEqual(["up"]);
  });

  test("a second press of a held control is refused, so nothing is left down", () => {
    const holds = new ButtonHolds();
    expect(holds.press("ok")).toBe(true);
    expect(holds.press("ok")).toBe(false);
    holds.letGo("ok");
    // Still down for the guest until its release goes out.
    expect(holds.press("ok")).toBe(false);
  });

  test("letting go of something that was never pressed, or twice, reports nothing", () => {
    const holds = new ButtonHolds();
    expect(holds.letGo("up")).toBe(false);
    holds.press("up");
    expect(holds.letGo("up")).toBe(true);
    expect(holds.letGo("up")).toBe(false);
  });

  test("losing focus lets go of everything that was held", () => {
    const holds = new ButtonHolds();
    holds.press("up");
    holds.press("power");
    holds.pressed("up", 0n);
    holds.pressed("power", 0n);
    expect(holds.letGoAll().sort()).toEqual(["power", "up"]);
    expect(holds.due(MIN_HOLD_PS).sort()).toEqual(["power", "up"]);
  });

  test("a refused press and a cleared machine leave nothing to release", () => {
    const holds = new ButtonHolds();
    holds.press("ok");
    holds.forget("ok");
    expect(holds.isDown("ok")).toBe(false);
    holds.press("down");
    holds.pressed("down", 0n);
    holds.letGo("down");
    holds.clear();
    expect(holds.due(MIN_HOLD_PS)).toEqual([]);
  });
});

describe("the USB connector", () => {
  // The strip routes into the USB card's `select`. A second, absolute builder here once re-sent the
  // current state, which is a re-enumeration rather than a no-op.
  test("the strip has no absolute U-state builder of its own", async () => {
    const source = await Bun.file(new URL("./controls.ts", import.meta.url)).text();
    expect(source).not.toContain("export function usbStateArgs");
  });

  test("selecting the state the machine is already in sends nothing", () => {
    const state = usb.DEFAULT_USB;
    expect(usb.toArgs(state, usb.fromUsbState(usb.usbState(state), state))).toEqual([]);
  });

  test("U1 is not offered by any USB control, because the rail is the power button's", () => {
    expect(usb.RAIL_ONLY_STATES).toContain("U1");
  });
});

describe("the keyboard mapping", () => {
  test("the keyboard keys reach the device controls", () => {
    expect(keyAction({ key: "ArrowUp" })).toEqual({ kind: "button", button: "up" });
    expect(keyAction({ key: "ArrowDown" })).toEqual({ kind: "button", button: "down" });
    expect(keyAction({ key: "Enter" })).toEqual({ kind: "button", button: "ok" });
    expect(keyAction({ key: "p" })).toEqual({ kind: "power" });
    expect(keyAction({ key: "P" })).toEqual({ kind: "power" });
  });

  test("every device control has a binding", () => {
    const reached = new Set<string>();
    for (const binding of DEVICE_BINDINGS) {
      const action = keyAction({ key: binding.key });
      expect(action).not.toBeNull();
      if (action?.kind === "button") {
        reached.add(action.button);
      } else if (action?.kind === "power") {
        reached.add("power");
      }
    }
    expect([...reached].sort()).toEqual(["down", "ok", "power", "up"]);
  });

  test("a modifier combination belongs to the browser, not the device", () => {
    expect(keyAction({ key: "p", ctrlKey: true })).toBeNull();
    expect(keyAction({ key: "Enter", metaKey: true })).toBeNull();
    expect(keyAction({ key: "ArrowUp", altKey: true })).toBeNull();
  });

  test("a text field owns its own keystrokes, so Enter does not press OK", () => {
    expect(keyAction({ key: "Enter", inTextField: true })).toBeNull();
    expect(keyAction({ key: "p", inTextField: true })).toBeNull();
  });

  test("isTextField recognises the fields the page has", () => {
    expect(isTextField({ tagName: "INPUT" })).toBe(true);
    expect(isTextField({ tagName: "textarea" })).toBe(true);
    expect(isTextField({ tagName: "SELECT" })).toBe(true);
    expect(isTextField({ tagName: "DIV", isContentEditable: true })).toBe(true);
    expect(isTextField({ tagName: "BUTTON" })).toBe(false);
    expect(isTextField(null)).toBe(false);
  });

  test("an unbound key does nothing", () => {
    expect(keyAction({ key: "q" })).toBeNull();
    expect(keyAction({ key: "F5" })).toBeNull();
  });
});

describe("a press through the client", () => {
  test("pressing OK journals exactly `input {button: ok, action: press}`", async () => {
    const journal = new UiJournal();
    const sent: string[] = [];
    const client = new CommandClient(
      (request) => {
        sent.push(request);
        return Promise.resolve({ ok: '{"json":{},"text":""}' });
      },
      { journal, nowPs: () => 1_000_000_000n },
    );

    await client.call("input", edgeArgs("ok", true));

    expect(journal.list()).toHaveLength(1);
    expect(journal.last()?.command).toBe("input");
    expect(journal.last()?.args).toEqual({ button: "ok", action: "press" });
    expect(JSON.parse(sent[0] ?? "{}")).toEqual({
      cmd: "input",
      args: { button: "ok", action: "press" },
    });
  });
});
