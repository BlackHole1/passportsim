// The environment cards against real firmware, one Playwright test each, on every project
// `browsers.ts` makes on this host. Each drives the page as a person would: the firmware arrives
// through the loader (`harness.ts` `requireImage`), the machine runs `Wall`-paced, and every
// assertion is on something the page shows.
//
// A test skips only for the named absences of `preconditions.ts`; otherwise a wrong image, a
// refused load or a `status` error fails it. None is `test.fixme`, and none is faked.
//
// The `ui` tree listing needs the app ELF beside the image (`pemu_load` kind 2), so
// `PEMU_E2E_IMAGE_DEMO` must name the corpus directory holding the merged bin and
// `FoloToy-AI-Passport.elf`. The controls test needs no image.

import { expect, test, type Page } from "@playwright/test";
import {
  browserGaps,
  call,
  consoleText,
  holdControl,
  openCard,
  openPage,
  requireImage,
  sendConsoleLine,
  showTab,
  virtualUs,
  waitForLine,
} from "./harness";

/** Guest time after a click's release before the next press (`openGuestWifiCard`). */
const CLICK_GAP_US = 300_000;

// A test whose browser is not installed skips with the install hint rather than failing.
for (const { applies, gap } of browserGaps()) {
  test.skip(({ browserName, channel }) => applies(browserName, channel), gap);
}

const GUEST_MS = 40_000;

test.describe("the environment cards against real firmware", () => {
  test("pk with the USB card at U3 reaches `pk_app: ready` in the page console", async ({ page }) => {
    await openPage(page);
    await requireImage(page, "pk", { name: "the pk boot", waitsOn: "the pk boot" });
    const usb = await openCard(page, "usb");
    await usb.locator('[data-usb-card="U3"]').click();
    await showTab(page, "console");
    await waitForLine(page, /pk_app: ready/, GUEST_MS);
    expect(await consoleText(page), "no panic on the way to ready").not.toMatch(/Guru Meditation|panic/i);
  });

  test("the BLE card's virtual central completes the command exchange against pk", async ({ page }) => {
    // The `pk` BLE exchange as a person drives it: scan, connect, discover, subscribe to events and
    // write two command lines to the commands characteristic, reading each answer off the card.
    test.setTimeout(180_000);
    const VENDOR = "12D4FA08-7418-48FA-A95A-B43A2E669E55";
    const EVENTS = "12D4FA09-7418-48FA-A95A-B43A2E669E55";
    const COMMANDS = "12D4FA0A-7418-48FA-A95A-B43A2E669E55";
    await openPage(page);
    await requireImage(page, "pk", { name: "the BLE card", waitsOn: "the pk BLE exchange" });
    // The firmware advertises once its application is up (`pk_app: ready`).
    await showTab(page, "console");
    await waitForLine(page, /pk_app: ready/, GUEST_MS);
    const ble = await openCard(page, "ble");

    await ble.getByRole("button", { name: "Scan", exact: true }).click();
    await expect(ble.locator("ul.peer-list li")).not.toHaveCount(0, { timeout: GUEST_MS });
    await ble.locator("ul.peer-list li").first().getByRole("button", { name: "connect" }).click();
    await ble.getByRole("button", { name: "Discover", exact: true }).click();
    await expect(ble.locator("ul.gatt-tree")).toContainText(VENDOR, { timeout: GUEST_MS });

    const chooser = ble.getByLabel("Characteristic");
    const value = ble.getByLabel(/value/i);
    const write = ble.getByRole("button", { name: /write/i });

    await chooser.selectOption(EVENTS);
    await ble.getByRole("button", { name: /subscribe/i }).click();
    // The card states the link it holds, which is how this test knows the CCCD write landed.
    await expect(ble.locator('[data-ble="link"]')).toContainText(`subscribed ${EVENTS}`, { timeout: GUEST_MS });

    // `pk_protocol.c` frames on the newline, so each command line carries one.
    const exchange: readonly (readonly [string, string])[] = [
      ['{"cmd":"hello"}\n', '"t":"hello"'],
      ['{"cmd":"ping"}\n', '{"t":"pong"}'],
    ];
    for (const [line, answer] of exchange) {
      await chooser.selectOption(COMMANDS);
      await value.fill(line);
      await write.click();
      await expect(ble.locator("ul.notification-list")).toContainText(answer, { timeout: GUEST_MS });
    }

    expect(await consoleText(page), "no host reset during the exchange").not.toMatch(/host reset/i);
  });

  test("loading a URI tag and tapping it increments the NFC counter", async ({ page }) => {
    await openPage(page);
    await requireImage(page, "pk", { name: "the NFC card", waitsOn: "the NFC counter" });
    const nfc = await openCard(page, "nfc");
    const counter = nfc.locator('[data-nfc="counter"]');

    // Arm NFC_CNT_EN, write the URI, then tap. Without the ACCESS bit the counter never moves.
    await nfc.locator('[data-action="nfc-counter"]').click();
    await expect(counter).toHaveText(/counter armed/);
    await nfc.getByLabel("URI").fill("https://example.com/passport");
    await nfc.locator('[data-action="ndef-add"]').click();
    await expect(nfc.locator("ul.ndef-list li")).toHaveCount(1);

    await nfc.locator('[data-action="tap"]').click();
    await expect(counter, "the first tap moves the counter to 1").toHaveText("counter 1", { timeout: GUEST_MS });
    await nfc.locator('[data-action="tap"]').click();
    await expect(counter, "one increment per tap").toHaveText("counter 2", { timeout: GUEST_MS });
  });

  test("USB U3 to U0 for 5 s and back gives link-state lines; power hold and press", async ({ page }) => {
    test.setTimeout(180_000);
    await openPage(page);
    await requireImage(page, "pk", { name: "the power and USB cards", waitsOn: "the power and USB link-state lines" });
    const usb = await openCard(page, "usb");
    await showTab(page, "console");
    // The test moves a running `pk`, so it waits for the first link-state line (at 662 ms virtual).
    await waitForLine(page, /pk_app: link state -1 -> 0/, GUEST_MS);

    // A cable alone moves no link state: `pk_app.c` `refresh_link_state` picks PK_UI_LINK_USB (2) only
    // while the Mac app has sent a command within 15 s. The console input plays that app with
    // `{"cmd":"hello"}`. It is re-sent until the line lands, because the page's `serial` write is
    // fire-and-forget and a write before the port opens is refused silently.
    const helloUntilLink = async (since: () => Promise<string>) => {
      await expect
        .poll(
          async () => {
            await sendConsoleLine(page, '{"cmd":"hello"}');
            await page.waitForTimeout(1_000);
            return since();
          },
          { timeout: GUEST_MS, intervals: [0] },
        )
        .toMatch(/pk_app: link state 0 -> 2/);
    };
    const fromBoot = await consoleText(page);
    await helloUntilLink(async () => (await consoleText(page)).slice(fromBoot.length));

    // The page paces at rate 1, so 5 s of the user's time is 5 s of the guest's.
    const beforeUnplug = (await consoleText(page)).length;
    await usb.locator('[data-usb-card="U0"]').click();
    await page.waitForTimeout(5_000);
    await usb.locator('[data-usb-card="U3"]').click();
    // What the guest printed while the link was down never left the endpoint, so the `2 -> 0` line
    // cannot appear; the handshake over the restored link does.
    await helloUntilLink(async () => (await consoleText(page)).slice(beforeUnplug));
    expect(await consoleText(page), "no panic across the cable change").not.toMatch(/Guru Meditation|panic/i);

    // 2.2 s of power stops the console; a 0.6 s press brings a `rst:0x1` banner.
    await holdControl(page, "power", 2_200);
    await page.waitForTimeout(1_000);
    const stopped = await consoleText(page);
    await page.waitForTimeout(3_000);
    expect(await consoleText(page), "the console stops while the rail is off").toBe(stopped);
    await holdControl(page, "power", 600);
    await expect
      .poll(async () => (await consoleText(page)).slice(stopped.length), { timeout: GUEST_MS })
      .toMatch(/rst:0x1/);
  });

  /** The three scripted access points, in the order the firmware draws them: strongest first. */
  const DEMO_APS = [
    { ssid: "G2-Alpha", rssi: -42, channel: 1 },
    { ssid: "G2-Bravo", rssi: -60, channel: 6 },
    { ssid: "G2-Charlie", rssi: -75, channel: 11 },
  ] as const;

  /** The rows `demo_wifi.c` `show_scan_results` draws, strongest first. */
  const DEMO_AP_ROWS = ["-42  G2-Alpha  ch1", "-60  G2-Bravo  ch6", "-75  G2-Charlie  ch11"];

  /**
   * The Wi-Fi console lines the HLE synthesizes through the image's own `esp_log`
   * (`specs/hle/idf-5.5.3/log-lines.toml`), in order, matched after the guest's timestamps. The
   * station MAC is the guest's placeholder (`02:00:00`).
   */
  const WIFI_LOG_LINES = [
    "pp: pp rom version: 74f9620",
    "wifi:wifi driver task: ",
    "wifi:wifi firmware version: 4df78f2",
    "wifi_init: WiFi RX IRAM OP enabled",
    "wifi:mode : sta (02:00:00:",
    "wifi:enable tsf",
  ];

  /** Fills the card with `DEMO_APS`, which sends the whole air through `env` (`panels/wifi.ts`). */
  async function scriptTheAir(page: Page): Promise<void> {
    const card = await openCard(page, "wifi");
    for (const ap of DEMO_APS) {
      await card.getByLabel("SSID").fill(ap.ssid);
      await card.getByLabel("Channel").fill(String(ap.channel));
      await card.getByLabel("RSSI").fill(String(ap.rssi));
      await card.getByRole("button", { name: "Add AP" }).click();
    }
    await expect(card.locator("ul.ap-list li")).toHaveCount(DEMO_APS.length);
  }

  /**
   * Opens the guest's Wi-Fi card: four `down` clicks and one `ok` from the menu, the fifth of seven
   * cards (`main.c` `DEMOS[]`). `iot_button` reports a click about 185 ms after release and a press
   * inside that window is a double click, so each click gets 300 ms of guest time.
   */
  async function openGuestWifiCard(page: Page): Promise<void> {
    const click = async (control: string) => {
      await holdControl(page, control, 80);
      const released = (await virtualUs(page)) ?? 0;
      await expect.poll(async () => (await virtualUs(page)) ?? 0, { timeout: GUEST_MS }).toBeGreaterThanOrEqual(released + CLICK_GAP_US);
    };
    for (let count = 0; count < 4; count += 1) {
      await click("down");
    }
    await click("ok");
  }

  test("the Wi-Fi card's scripted APs are listed by demo in the ui tree and console", async ({ page }) => {
    // `ui` walks the guest's LVGL globals against the app ELF's DWARF, which the wasm core resolves
    // from the ELF loaded as `pemu_load` kind 2. Without an ELF beside the bin this fails naming it.
    test.setTimeout(180_000);
    await openPage(page);
    await requireImage(page, "demo", { name: "the Wi-Fi card", waitsOn: "the scripted Wi-Fi scan" });
    await waitForLine(page, /main: 就绪/, GUEST_MS);
    await scriptTheAir(page);
    await openGuestWifiCard(page);

    const tree = async () => {
      const answer = await call(page, "ui", {});
      return answer.ok ? JSON.stringify(answer.json) : answer.error;
    };
    await expect.poll(tree, { timeout: GUEST_MS }).toContain("3 APs  |  OK: RESCAN");
    const listed = await tree();
    expect(listed, "the five clicks opened the guest's Wi-Fi card").toContain("WI-FI SCAN");
    for (const row of DEMO_AP_ROWS) {
      expect(listed, "one row per scripted access point, strongest first").toContain(row);
    }
  });

  // The console half: the card's APs reach the machine through `env`, the guest starts Wi-Fi, and
  // the console shows the bring-up with no failure line. A separate test, so a broken `ui` walk
  // cannot hide whether the air reached the machine.
  test("the card's air reaches demo, which brings Wi-Fi up with no failure line", async ({ page }) => {
    await openPage(page);
    await requireImage(page, "demo", { name: "the Wi-Fi card", waitsOn: "the scripted Wi-Fi scan" });
    await waitForLine(page, /main: 就绪/, GUEST_MS);
    await scriptTheAir(page);
    await openGuestWifiCard(page);
    await showTab(page, "console");

    // The bring-up, in order, after the menu line: everything before it is the boot.
    await expect.poll(async () => (await consoleText(page)).includes("wifi:enable tsf"), { timeout: GUEST_MS }).toBe(true);
    const text = await consoleText(page);
    const card = text.slice(text.indexOf("main: 就绪"));
    let at = 0;
    for (const line of WIFI_LOG_LINES) {
      const found = card.indexOf(line, at);
      expect(found, `the console has no \`${line}\` line after the one before it`).toBeGreaterThanOrEqual(0);
      at = found + line.length;
    }
    // `demo_wifi.c` prints `E (t) demo_wifi: Wi-Fi ...: <code>` for any failed `esp_wifi_*` call.
    expect(card, "the card printed no Wi-Fi failure").not.toMatch(/demo_wifi/);
    expect(card, "and no error line at all").not.toMatch(/\bE \(\d+\)/);
  });

  test("at 400 px every environment card fits with no horizontal page scroll", async ({ page }) => {
    await openPage(page, 400, 800);
    const noPageScroll = () =>
      page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth && document.body.scrollWidth <= window.innerWidth);
    expect(await noPageScroll()).toBe(true);

    // The skin's power and USB controls.
    for (const selector of ['[data-control="power"]', '[data-usb="U0"]', '[data-usb="U3"]']) {
      const node = page.locator(selector);
      await node.scrollIntoViewIfNeeded();
      const box = await node.boundingBox();
      expect(box, selector).not.toBeNull();
      expect((box?.x ?? -1) >= 0 && (box?.x ?? 0) + (box?.width ?? 0) <= 400, selector).toBe(true);
    }

    // The loader, which every other test needs for its image.
    for (const selector of ['[data-loader-input="files"]', '[data-loader-input="directory"]']) {
      const node = page.locator(selector);
      await node.scrollIntoViewIfNeeded();
      const box = await node.boundingBox();
      expect(box, selector).not.toBeNull();
      expect((box?.x ?? -1) >= 0 && (box?.x ?? 0) + (box?.width ?? 0) <= 400, selector).toBe(true);
    }

    // Each card, expanded, and its controls actionable inside the viewport.
    const controls: Record<string, string[]> = {
      usb: ['[data-usb-card="U0"]', '[data-usb-card="U3"]'],
      ble: ['button:text-is("Scan")', 'button:text-is("Discover")'],
      nfc: ['[data-action="ndef-add"]', '[data-action="nfc-counter"]', '[data-action="tap"]'],
      wifi: ['button:text-is("Add AP")'],
    };
    for (const [card, selectors] of Object.entries(controls)) {
      const section = await openCard(page, card);
      for (const selector of selectors) {
        const node = section.locator(selector).first();
        await node.scrollIntoViewIfNeeded();
        await node.click({ trial: true });
        const box = await node.boundingBox();
        expect((box?.x ?? -1) >= 0 && (box?.x ?? 0) + (box?.width ?? 0) <= 400, `${card} ${selector}`).toBe(true);
      }
      expect(await noPageScroll(), `after opening the ${card} card`).toBe(true);
    }

    // Each card control reaches the registry, which the Events tab lists.
    await (await openCard(page, "usb")).locator('[data-usb-card="U2"]').click();
    await (await openCard(page, "ble")).getByRole("button", { name: "Scan", exact: true }).click();
    await holdControl(page, "power", 100);
    await showTab(page, "events");
    const names = await page.locator('#pane-events tbody tr[data-source="ui"] td:nth-child(3)').allTextContents();
    expect(names).toEqual(expect.arrayContaining(["input", "ble_scan"]));
    expect(await noPageScroll()).toBe(true);
  });
});
