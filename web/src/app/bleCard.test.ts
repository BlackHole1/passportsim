import { describe, expect, test } from "bun:test";
import { createPage } from "./page";
import { installDom, settle } from "./view/testDom";

const window = installDom();
const document = window.document as unknown as Document;

type Answer = { readonly json: unknown } | { readonly error: { code: string; message: string; detail?: unknown } };
type Answers = (cmd: string, args: Record<string, unknown>) => Answer;

interface Mounted {
  readonly mount: HTMLElement;
  readonly card: HTMLElement;
  readonly sent: { cmd: string; args: Record<string, unknown> }[];
}

async function mountCard(answers: Answers, locale = "en-US"): Promise<Mounted> {
  const mount = document.createElement("div");
  document.body.appendChild(mount);
  const sent: { cmd: string; args: Record<string, unknown> }[] = [];
  let clock = 0;
  const page = createPage({
    mount,
    transport: (request) => {
      const call = JSON.parse(request) as { cmd: string; args: Record<string, unknown> };
      sent.push(call);
      const answer = answers(call.cmd, call.args);
      return Promise.resolve("error" in answer ? { err: JSON.stringify(answer.error) } : { ok: JSON.stringify({ json: answer.json, text: "" }) });
    },
    toWorker: () => {},
    rewindSource: { snapshot: () => new Uint8Array(0), frame: () => null },
    now: () => (clock += 3),
    confirm: () => true,
    viewport: () => ({ width: 1440, height: 900 }),
    devicePixelRatio: () => 1,
    storage: () => null,
    search: "?mode=advanced",
    systemLocale: () => locale,
  });
  page.ready("demo");
  await settle();
  await settle();
  return { mount, card: mount.querySelector("[data-card=ble]") as HTMLElement, sent };
}

function button(card: HTMLElement, label: string): HTMLButtonElement {
  const found = [...card.querySelectorAll("button")].find((node) => node.textContent?.trim() === label);
  if (!found) {
    throw new Error(`no button labelled ${label}`);
  }
  return found as HTMLButtonElement;
}

const DEMO_ADV = { addr: "02:00:00:78:4D:22", random: false, pdu: "ADV_SCAN_IND", connectable: false, name: "FoloPassport" };
const DEMO_RADIO = { state: "advertising", advertising: DEMO_ADV };
const NON_CONNECTABLE = {
  code: "E_STATE",
  message: "the service discovery needs a connection, and 02:00:00:78:4D:22 advertises ADV_SCAN_IND",
  detail: { ble: "non_connectable", pdu: "ADV_SCAN_IND", addr: "02:00:00:78:4D:22" },
};

/** The demo on its BLE page: scannable, not connectable. */
const demo: Answers = (cmd, args) => {
  if (cmd === "ble_scan") {
    return args.duration_ms === 0
      ? { json: { outcome: "read", found: [], heard: 0, radio: DEMO_RADIO } }
      : {
          json: {
            outcome: "heard",
            heard: 1,
            adv_events: 12,
            radio: DEMO_RADIO,
            found: [{ addr: DEMO_ADV.addr, name: "FoloPassport", pdu: "ADV_SCAN_IND", connectable: false, heard: true }],
          },
        };
  }
  if (cmd === "ble_gatt" || cmd === "ble_connect") {
    return { error: NON_CONNECTABLE };
  }
  return { json: {} };
};

describe("the BLE card", () => {
  test("reads the radio's state without a journaled call, and says the demo accepts no connection", async () => {
    const { mount, card, sent } = await mountCard(demo);
    const line = card.querySelector("[data-ble-state]") as HTMLElement;
    expect(line.getAttribute("data-ble-state")).toBe("advertising-non-connectable");
    expect(line.textContent).toContain("Advertising FoloPassport as ADV_SCAN_IND, not connectable");
    expect(sent.some((call) => call.cmd === "ble_scan" && call.args.duration_ms === 0)).toBe(true);
    // The read is the page's own: the Events tab lists only what the user did.
    const events = mount.querySelector("#pane-events") as HTMLElement;
    expect(events.textContent).not.toContain("ble_scan");
    button(card, "Scan").click();
    await settle();
    await settle();
    expect(events.textContent).toContain("ble_scan");
  });

  test("lists a scan's advertiser with its PDU and marker, and offers no connect to a non-connectable one", async () => {
    const { card } = await mountCard(demo);
    button(card, "Scan").click();
    await settle();
    await settle();
    const row = card.querySelector("ul.peer-list li") as HTMLElement;
    expect(row.textContent).toContain("FoloPassport");
    expect(row.textContent).toContain("02:00:00:78:4D:22");
    expect(row.textContent).toContain("ADV_SCAN_IND");
    expect(row.textContent).toContain("not connectable");
    expect(row.getAttribute("data-connectable")).toBe("false");
    expect([...row.querySelectorAll("button")].map((node) => node.textContent)).not.toContain("connect");
    expect(card.querySelector("[data-ble-scan]")?.textContent).toBe("The scan heard 1 advertiser(s).");
  });

  test("Discover on a non-connectable advertiser says why instead of an error string", async () => {
    const { mount, card } = await mountCard(demo);
    button(card, "Discover").click();
    await settle();
    await settle();
    const why = card.querySelector("[data-ble-why]") as HTMLElement;
    expect(why.getAttribute("data-ble-why")).toBe("non_connectable");
    expect(why.textContent).toBe(
      "02:00:00:78:4D:22 advertises ADV_SCAN_IND: the firmware does not accept connections, so the central cannot connect to it or discover its services.",
    );
    expect((card.querySelector(".card-error") as HTMLElement).hidden).toBe(true);
    const log = [...mount.querySelectorAll(".console .log-page")].map((node) => node.textContent ?? "");
    expect(log.at(-1)).toContain("ble_gatt was refused: E_STATE");
  });

  test("before the firmware starts Bluetooth, the card waits for it rather than calling the radio unbound", async () => {
    const { card } = await mountCard((cmd) =>
      cmd === "ble_scan"
        ? { error: { code: "E_STATE", message: "the BLE module is bound, but ...", detail: { ble: "not_started", binding: "bound" } } }
        : { json: {} },
    );
    const line = card.querySelector("[data-ble-state]") as HTMLElement;
    expect(line.getAttribute("data-ble-state")).toBe("not_started");
    expect(line.textContent).toContain("Waiting for the firmware to start Bluetooth");
    expect(card.querySelector("[data-radio-unbound]")).toBeNull();
  });

  test("a connectable advertiser is offered a connect, and the state is said in Chinese", async () => {
    const pk = { addr: "02:00:00:12:34:56", random: false, pdu: "ADV_IND", connectable: true, name: "Passport Keys" };
    const { card } = await mountCard(
      (cmd) =>
        cmd === "ble_scan"
          ? {
              json: {
                outcome: "heard",
                heard: 1,
                adv_events: 3,
                radio: { state: "advertising", advertising: pk },
                found: [{ ...pk, heard: true }],
              },
            }
          : { json: {} },
      "zh-CN",
    );
    expect(card.querySelector("[data-ble-state]")?.textContent).toBe("正在以 ADV_IND 广播 Passport Keys，可连接。");
    button(card, "扫描").click();
    await settle();
    await settle();
    const row = card.querySelector("ul.peer-list li") as HTMLElement;
    expect(row.getAttribute("data-connectable")).toBe("true");
    expect([...row.querySelectorAll("button")].map((node) => node.textContent)).toContain("连接");
    // Once connected, the same advertiser is not offered a second connection.
    button(row, "连接").click();
    await settle();
    await settle();
    expect([...(card.querySelector("ul.peer-list li") as HTMLElement).querySelectorAll("button")].map((node) => node.textContent)).not.toContain("连接");
  });
});
