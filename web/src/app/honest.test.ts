import { describe, expect, test } from "bun:test";
import { StopCode } from "../worker/layout";
import { createPage, type Page } from "./page";
import { PREF_KEYS, type StorageLike } from "./prefs";
import { CSS_PX_PER_MM } from "./skinGeometry";
import { installDom, settle, settleUntil } from "./view/testDom";

const window = installDom();
const document = window.document as unknown as Document;

interface Mounted {
  readonly page: Page;
  readonly mount: HTMLElement;
  readonly toWorker: unknown[];
  readonly requests: string[];
}

type Refusals = Readonly<Record<string, { code: string; message: string }>>;

function mountPage(options: { storage?: StorageLike; search?: string; refuse?: Refusals } = {}): Mounted {
  const mount = document.createElement("div");
  document.body.appendChild(mount);
  const toWorker: unknown[] = [];
  const requests: string[] = [];
  let clock = 0;
  const page = createPage({
    mount,
    transport: (request) => {
      requests.push(request);
      const cmd = (JSON.parse(request) as { cmd: string }).cmd;
      const refusal = options.refuse?.[cmd];
      return Promise.resolve(refusal ? { err: JSON.stringify(refusal) } : { ok: JSON.stringify({ json: {}, text: "" }) });
    },
    toWorker: (message) => toWorker.push(message),
    rewindSource: { snapshot: () => new Uint8Array(0), frame: () => null },
    now: () => (clock += 3),
    confirm: () => true,
    viewport: () => ({ width: 1440, height: 900 }),
    devicePixelRatio: () => 1,
    storage: () => options.storage ?? null,
    search: options.search,
    systemLocale: () => "en-US",
  });
  return { page, mount, toWorker, requests };
}

function memory(): StorageLike & { readonly map: Map<string, string> } {
  const map = new Map<string, string>();
  return {
    map,
    getItem: (key) => map.get(key) ?? null,
    setItem: (key, value) => {
      map.set(key, value);
    },
  };
}

const TRIPWIRE = JSON.stringify({ reason: "Tripwire", code: StopCode.Tripwire, detail: "Tripwire(TripReport { .. })", vt_ps: "411000000000" });

const pageLines = (mount: HTMLElement) => [...mount.querySelectorAll(".log-panel .log-page")].map((line) => line.textContent ?? "");

function key(name: string): KeyboardEvent {
  return new window.KeyboardEvent("keydown", { key: name }) as unknown as KeyboardEvent;
}

describe("a machine that stopped by itself", () => {
  test("is stated in the status, the log and the controls, and input is not delivered", async () => {
    const { page, mount, requests } = mountPage();
    page.ready("demo");
    await settle();
    expect(mount.querySelector(".sim-status [data-state]")?.getAttribute("data-state")).toBe("running");
    expect(mount.querySelector("[data-stop]")).toBeNull();

    page.stopped(StopCode.Tripwire, TRIPWIRE);
    await settle();
    const notice = mount.querySelector("[data-stop]");
    expect(notice?.getAttribute("data-stop")).toBe("Tripwire");
    expect(notice?.textContent).toContain("The machine stopped: the firmware reached hardware this machine does not provide");
    expect(notice?.textContent).toContain("Stopped at vt 0.411 s.");
    expect(notice?.textContent).toContain("Buttons and keys are not delivered while the machine is stopped.");
    expect(notice?.querySelector(".stop-detail")?.textContent).toContain("Tripwire(TripReport");
    // A fault offers a restart and not a continue, which would stop again at once.
    expect(notice?.querySelector('[data-action="restart"]')).not.toBeNull();
    expect(notice?.querySelector('[data-action="continue"]')).toBeNull();
    expect(mount.querySelector(".sim-status [data-state]")?.getAttribute("data-state")).toBe("stopped");
    expect(pageLines(mount).at(-1)).toContain("Machine stopped at vt 0.411 s: the firmware reached hardware this machine does not provide (Tripwire)");
    for (const id of ["up", "ok", "down", "power"]) {
      expect((mount.querySelector(`[data-control=${id}]`) as HTMLButtonElement).disabled).toBe(true);
    }

    const before = requests.length;
    expect(page.key(key("ArrowDown"), true)).toBe(true);
    page.key(key("ArrowDown"), false);
    await settle();
    expect(requests.slice(before).filter((request) => request.includes('"cmd":"input"'))).toEqual([]);
  });

  test("a core error under the pacing loop is a stop with a restart, not a refusal", async () => {
    const { page, mount } = mountPage();
    page.ready("demo");
    await settle();
    page.fatal("RuntimeError: unreachable executed", "E_INTERNAL");
    await settle();
    const notice = mount.querySelector("[data-stop]");
    expect(notice?.getAttribute("data-stop")).toBe("E_INTERNAL");
    expect(notice?.textContent).toContain("The machine stopped: the emulator core failed");
    expect(notice?.querySelector(".stop-detail")?.textContent).toContain("RuntimeError: unreachable executed");
    expect(notice?.querySelector('[data-action="restart"]')).not.toBeNull();
    expect(notice?.querySelector('[data-action="continue"]')).toBeNull();
    expect(mount.querySelector(".sim-status [data-state]")?.getAttribute("data-state")).toBe("stopped");
    expect((mount.querySelector("[data-control=ok]") as HTMLButtonElement).disabled).toBe(true);
  });

  test("a breakpoint offers to continue, and the next machine clears the stop", async () => {
    const { page, mount } = mountPage();
    page.ready("demo");
    await settle();
    page.stopped(StopCode.Breakpoint, null);
    await settle();
    expect(mount.querySelector('[data-stop] [data-action="continue"]')).not.toBeNull();

    page.ready();
    await settle();
    expect(mount.querySelector("[data-stop]")).toBeNull();
    expect((mount.querySelector("[data-control=ok]") as HTMLButtonElement).disabled).toBe(false);
  });
});

describe("an image loaded without its ELF", () => {
  function mergedBin(): Uint8Array {
    const bytes = new Uint8Array(0x20_000);
    bytes[0] = 0xe9;
    return bytes;
  }

  test("the firmware card says which features need it and how to provide it; the demo says nothing", async () => {
    const { page, mount, toWorker } = mountPage();
    page.ready("demo");
    await settle();
    expect(mount.querySelector("[data-elf-hint]")).toBeNull();

    const offered = page.loader.offer({
      root: null,
      files: [{ path: "keys.bin", size: 0x20_000, read: () => Promise.resolve(mergedBin()) }],
    });
    await settle();
    await settle();
    const boot = toWorker.find((message) => (message as { type?: string }).type === "boot") as { token: string };
    page.ready(boot.token);
    await offered;
    await settle();
    const hint = mount.querySelector(".firmware-card [data-elf-hint]");
    expect(hint?.textContent).toContain("The UI tree and settle detection need the application's ELF");
    expect(hint?.textContent).toContain("drop the whole idf.py build folder");
    expect(hint?.textContent).toContain(".pebundle that carries the ELF");

    const back = page.loader.backToDemo();
    await settle();
    await settle();
    const demo = toWorker.filter((message) => (message as { type?: string }).type === "boot").at(-1) as { token: string };
    page.ready(demo.token);
    await back;
    await settle();
    expect(mount.querySelector("[data-elf-hint]")).toBeNull();
  });
});

describe("a radio with no module bound", () => {
  const NO_BLE = { code: "E_STATE", message: "this instance has no bound BLE module, so it has no virtual central" };

  test("the BLE card shows it as its state, and the raw refusal goes to the log", async () => {
    const { page, mount } = mountPage({ search: "?mode=advanced", refuse: { ble_scan: NO_BLE } });
    page.ready("demo");
    await settle();
    const card = mount.querySelector("[data-card=ble]") as HTMLElement;
    const scan = [...card.querySelectorAll("button")].find((button) => button.textContent === "Scan") as HTMLButtonElement;
    scan.click();
    await settle();
    await settle();
    expect(card.querySelector("[data-radio-unbound=ble]")?.textContent).toContain("No Bluetooth module is bound to this machine.");
    expect((card.querySelector(".card-error") as HTMLElement).hidden).toBe(true);
    const steps = [...mount.querySelectorAll(".console .log-page")].map((line) => line.textContent ?? "");
    expect(steps.at(-1)).toContain("ble_scan was refused: E_STATE: this instance has no bound BLE module");

    // Another machine may bind one: what the card learned was about the last.
    page.ready();
    await settle();
    expect(card.querySelector("[data-radio-unbound]")).toBeNull();
  });

  test("any other refusal stays on the card's error line", async () => {
    const { page, mount } = mountPage({ search: "?mode=advanced", refuse: { ble_scan: { code: "E_LEASE", message: "an agent holds the lease" } } });
    page.ready("demo");
    await settle();
    const card = mount.querySelector("[data-card=ble]") as HTMLElement;
    ([...card.querySelectorAll("button")].find((button) => button.textContent === "Scan") as HTMLButtonElement).click();
    await settleUntil(() => (card.querySelector(".card-error")?.textContent ?? "") !== "");
    expect(card.querySelector("[data-radio-unbound]")).toBeNull();
    expect(card.querySelector(".card-error")?.textContent).toContain("E_LEASE");
  });
});

describe("the device view", () => {
  test("draws the device fitted, never under 140 %, with its side buttons as the controls, and remembers another zoom", async () => {
    const storage = memory();
    const { page, mount } = mountPage({ storage });
    await settle();
    const body = mount.querySelector("[data-device-body]") as HTMLElement;
    expect(body).not.toBeNull();
    // Fit on a 1440 x 900 window: larger than 140 % of 60 CSS millimetres.
    expect(parseFloat(body.style.width)).toBeGreaterThan(60 * CSS_PX_PER_MM * 1.4);
    expect(mount.querySelector("[data-zoom]")?.getAttribute("data-zoom")).toBe("fit");
    expect(storage.map.size).toBe(0);
    // The glass is the page's one canvas, inside the body; each button is on the body once.
    expect(body.querySelector(".skin-screen canvas")).toBe(page.canvas);
    for (const id of ["up", "ok", "down", "power"]) {
      expect(mount.querySelectorAll(`[data-control=${id}]`).length).toBe(1);
      expect(body.querySelector(`[data-control=${id}]`)).not.toBeNull();
    }
    // Nothing of the old true-size view is left: no calibration, no size switch.
    expect(mount.querySelector('[data-action="real-size"], [data-action="calibrate"], [data-calibration]')).toBeNull();

    (mount.querySelector('[data-zoom-choice="180"]') as HTMLButtonElement).click();
    await settle();
    expect(storage.map.get(PREF_KEYS.zoom)).toBe("180");
    const larger = mount.querySelector("[data-device-body]") as HTMLElement;
    expect(parseFloat(larger.style.width)).toBeGreaterThan(60 * CSS_PX_PER_MM * 1.7);
    expect(larger.querySelector(".skin-screen canvas")).toBe(page.canvas);
  });

  test("?zoom= opens at that zoom without storing it, and an old ?size=real opens at the default", async () => {
    const storage = memory();
    const chosen = mountPage({ storage, search: "?zoom=100" });
    await settle();
    expect(chosen.mount.querySelector("[data-zoom]")?.getAttribute("data-zoom")).toBe("100");
    const old = mountPage({ storage, search: "?size=real" });
    await settle();
    expect(old.mount.querySelector("[data-zoom]")?.getAttribute("data-zoom")).toBe("fit");
    expect(storage.map.size).toBe(0);
  });
});
