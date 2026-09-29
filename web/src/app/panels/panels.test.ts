// Each card's state becomes command arguments and reads back as the same state, so a session
// replayed from the journal puts the cards back where the user had them.

import { describe, expect, test } from "bun:test";
import { CommandClient } from "../../api/client";
import { COMMAND_GROUP } from "../../api/commands";
import { CommandError } from "../../api/envelope";
import { UiJournal } from "../../api/journal";
import * as audio from "./audio";
import * as ble from "./ble";
import * as battery from "./battery";
import * as nfc from "./nfc";
import * as usb from "./usb";
import * as wifi from "./wifi";

/**
 * Outside the `02:00:00` prefix, for the cards to refuse. Bit 0 of the first octet makes it a
 * group address, so it cannot be any real device's address.
 */
const NOT_A_PLACEHOLDER = "03:11:22:33:44:55";

describe("battery", () => {
  test("each control sends its own field and nothing else", () => {
    expect(battery.changeArgs({ kind: "soc", soc: 20 })).toEqual({ battery: { soc: 20 } });
    expect(battery.changeArgs({ kind: "mv", mv: 3_700 })).toEqual({ battery: { mv: 3_700 } });
    expect(battery.changeArgs({ kind: "temp", tempC: 31 })).toEqual({ battery: { temp_c: 31 } });
    expect(battery.changeArgs({ kind: "connected", connected: false })).toEqual({ battery: { present: false } });
  });

  test("a charge moves the voltage and a voltage moves the charge, along the one curve", () => {
    const at20 = battery.applyChange(battery.DEFAULT_BATTERY, { kind: "soc", soc: 20 });
    expect(at20).toEqual({ ...battery.DEFAULT_BATTERY, soc: 20, mv: 3_620 });
    // 3700 mV is two fifths of the way from 30 % (3680) to 40 % (3730).
    const at3700 = battery.applyChange(at20, { kind: "mv", mv: 3_700 });
    expect(at3700).toEqual({ ...battery.DEFAULT_BATTERY, soc: 34, mv: 3_700 });
    for (let soc = 0; soc <= 100; soc += 1) {
      const cell = battery.applyChange(battery.DEFAULT_BATTERY, { kind: "soc", soc });
      expect(battery.socMilliAtOcv(cell.mv)).toBeGreaterThanOrEqual(soc === 0 ? 0 : (soc - 1) * 1_000);
      expect(battery.ocvMv(cell.soc * 1_000)).toBe(cell.mv);
    }
  });

  test("the card starts where a new board's cell does: full, at room temperature", () => {
    expect(battery.DEFAULT_BATTERY).toEqual({ soc: 100, mv: 4_200, tempC: 25, connected: true });
  });

  test("a disconnect carries no readings and leaves a full cell, as the board's reconnect does", () => {
    const low = battery.applyChange(battery.DEFAULT_BATTERY, { kind: "soc", soc: 10 });
    const args = battery.changeArgs({ kind: "connected", connected: false });
    expect(args).toEqual({ battery: { present: false } });
    expect(battery.fromArgs(args, low)).toEqual({ ...battery.DEFAULT_BATTERY, connected: false });
  });

  test("values outside the gauge's range are clamped, not sent", () => {
    expect(battery.changeArgs({ kind: "soc", soc: 400 })).toEqual({ battery: { soc: battery.SOC_RANGE.max } });
    expect(battery.changeArgs({ kind: "mv", mv: 39_000 })).toEqual({ battery: { mv: battery.MV_RANGE.max } });
    expect(battery.changeArgs({ kind: "temp", tempC: 900 })).toEqual({ battery: { temp_c: battery.TEMP_RANGE.max } });
  });

  test("a nonsense value falls back to the default rather than sending NaN", () => {
    expect(battery.changeArgs({ kind: "soc", soc: Number.NaN })).toEqual({ battery: { soc: battery.DEFAULT_BATTERY.soc } });
  });

  test("`fresh` is a disconnect and a reconnect, and carries no readings", () => {
    expect(battery.FRESH_ARGS).toEqual([
      { battery: { present: false } },
      { battery: { present: true } },
    ]);
  });

  test("the firmware's millivolts go through the gauge's 312.5 uV steps and back", () => {
    expect(battery.firmwareMv(3_620)).toBe(3_620);
    expect(battery.firmwareMv(4_200)).toBe(4_200);
    // 3701 mV is raw 11843, which the BSP prints as 3700.
    expect(battery.firmwareMv(3_701)).toBe(3_700);
  });

  test("arguments that name nothing leave the card where it was", () => {
    const base: battery.BatteryState = { soc: 55, mv: 3_815, tempC: 24, connected: true };
    expect(battery.fromArgs({}, base)).toEqual(base);
    expect(battery.fromArgs({ usb: "unplugged" }, base)).toEqual(base);
  });
});

describe("usb", () => {
  const triples: usb.UsbState[] = [
    { cable: false, rail: true, clientOpen: false },
    { cable: true, rail: false, clientOpen: false },
    { cable: true, rail: true, clientOpen: false },
    { cable: true, rail: true, clientOpen: true },
  ];

  test("each triple maps to its U-state, and back", () => {
    expect(triples.map(usb.usbState)).toEqual(["U0", "U1", "U2", "U3"]);
    for (const id of ["U0", "U1", "U2", "U3"] as const) {
      expect(usb.usbState(usb.fromUsbState(id))).toBe(id);
    }
  });

  test("a state change round-trips through the `input` calls it sends", () => {
    for (const from of triples) {
      for (const to of triples) {
        const landed = usb.applyAll(from, usb.toArgs(from, to));
        // The U-state is what round-trips, not the raw triple: U0 swallows the client flag, so an
        // unplugged card's stale `clientOpen` is not observable.
        expect(usb.usbState({ ...landed, rail: to.rail })).toBe(usb.usbState(to));
      }
    }
  });

  test("nothing is sent when nothing changed, because an input is journaled", () => {
    for (const state of triples) {
      expect(usb.toArgs(state, state)).toEqual([]);
    }
  });

  test("unplugging does not also send a client change the cable already implies", () => {
    const calls = usb.toArgs(
      { cable: true, rail: true, clientOpen: true },
      { cable: false, rail: true, clientOpen: false },
    );
    expect(calls).toEqual([{ button: "usb", action: "unplug" }]);
  });

  test("the card describes each state", () => {
    expect(usb.describe("U3")).toContain("client");
  });
});

describe("nfc", () => {
  const states: nfc.NfcState[] = [
    nfc.DEFAULT_NFC,
    {
      records: [{ type: "uri", uri: "https://example.com/p" }],
      uid: "04A1B2C3D4E5F6",
      locked: false,
      dwellMs: 300,
      counter: true,
    },
    {
      records: [
        { type: "text", text: "hello", lang: "en" },
        { type: "wifi", ssid: "TestAP", auth: "wpa2-personal", encr: "aes", key: "password1" },
      ],
      uid: null,
      locked: true,
      dwellMs: 900,
      counter: false,
    },
  ];

  test("every state round-trips through its `nfc_tag` arguments", () => {
    for (const state of states) {
      expect(nfc.fromArgs(nfc.toArgs(state), state)).toEqual(state);
    }
  });

  test("a malformed UID is not sent, and the seed's UID stays the default", () => {
    expect(nfc.toArgs({ ...nfc.DEFAULT_NFC, uid: "nope" }).uid).toBeUndefined();
    expect(nfc.toArgs({ ...nfc.DEFAULT_NFC, uid: null }).uid).toBeUndefined();
    expect(nfc.isValidUid("04A1B2C3D4E5F6")).toBe(true);
    expect(nfc.isValidUid("04A1B2C3D4E5")).toBe(false);
  });

  test("`lock` is only ever sent as true, because there is no unlock", () => {
    expect(nfc.toArgs({ ...nfc.DEFAULT_NFC, locked: false }).lock).toBeUndefined();
    expect(nfc.toArgs({ ...nfc.DEFAULT_NFC, locked: true }).lock).toBe(true);
  });

  test("a tap takes the card's dwell unless the form overrides it", () => {
    const state = { ...nfc.DEFAULT_NFC, dwellMs: 750 };
    expect(nfc.tapFor(state, { preset: "read" }).dwell_ms).toBe(750);
    expect(nfc.tapFor(state, { preset: "read", dwellMs: 40 }).dwell_ms).toBe(40);
  });

  test("a Wi-Fi record's key is never rendered into the card", () => {
    const line = nfc.describeRecord({
      type: "wifi",
      ssid: "TestAP",
      auth: "wpa2-personal",
      encr: "aes",
      key: "password1",
    });
    expect(line).not.toContain("password1");
    expect(line).toContain("9 chars");
  });
});

describe("wifi", () => {
  test("an AP round-trips through its `wifi_ap` arguments", () => {
    const ap = wifi.normalizeAp({ ssid: "TestAP", rssi: -55, channel: 6 }, 0);
    expect(wifi.fromArgs(wifi.toArgs(ap))).toEqual(ap);
  });

  test("a generated BSSID is a placeholder and is stable across sessions", () => {
    expect(wifi.placeholderBssid(0)).toBe("02:00:00:00:00:00");
    expect(wifi.placeholderBssid(1)).toBe("02:00:00:00:00:01");
    expect(wifi.placeholderBssid(258)).toBe("02:00:00:00:01:02");
    expect(wifi.placeholderBssid(0)).toBe(wifi.placeholderBssid(0));
    expect(wifi.isPlaceholderBssid(wifi.placeholderBssid(7))).toBe(true);
  });

  test("a BSSID outside the placeholder prefix is refused", () => {
    expect(() => wifi.normalizeAp({ ssid: "Home", bssid: NOT_A_PLACEHOLDER }, 0)).toThrow(
      wifi.WifiFormError,
    );
  });

  test("out-of-range fields are refused with the field that is wrong", () => {
    for (const [ap, field] of [
      [{ ssid: "" }, "ssid"],
      [{ ssid: "x".repeat(33) }, "ssid"],
      [{ ssid: "ok", channel: 14 }, "channel"],
      [{ ssid: "ok", rssi: 10 }, "rssi"],
    ] as const) {
      try {
        wifi.normalizeAp(ap, 0);
        throw new Error(`expected ${field} to be refused`);
      } catch (error) {
        expect((error as wifi.WifiFormError).field).toBe(field);
      }
    }
  });

  test("removing an AP names it and says so", () => {
    const ap = wifi.normalizeAp({ ssid: "TestAP" }, 3);
    expect(wifi.removeArgs(ap)).toEqual({ ssid: "TestAP", bssid: ap.bssid, remove: true });
  });

  // The card owns the whole list, so it sends `env`'s `wifi` section rather than `wifi_ap` per AP.
  test("the card scripts the whole air through `env`, with the auth mode the driver reports", () => {
    const first = wifi.normalizeAp({ ssid: "G2-Alpha", rssi: -42, channel: 1, auth: "open" }, 0);
    const second = wifi.normalizeAp({ ssid: "G2-Bravo", rssi: -60, channel: 6, key: "scripted-key" }, 1);
    expect(wifi.envArgs([first, second])).toEqual({
      wifi: {
        aps: [
          // No key is an open network, which is what `env` accepts without a `psk`.
          { ssid: "G2-Alpha", bssid: first.bssid, rssi: -42, channel: 1, auth: 0 },
          // A key makes it WPA2-PSK, which the driver reports as 3, and carries the key.
          { ssid: "G2-Bravo", bssid: second.bssid, rssi: -60, channel: 6, auth: 3, psk: "scripted-key" },
        ],
      },
    });
    expect(wifi.envArgs([])).toEqual({ wifi: { aps: [] } });
  });

  test("a key that is not a WPA2 key is refused before it is sent", () => {
    expect(() => wifi.normalizeAp({ ssid: "G2-Alpha", key: "short" }, 0)).toThrow(wifi.WifiFormError);
    expect(wifi.normalizeAp({ ssid: "G2-Alpha" }, 0).auth).toBe("open");
    expect(wifi.normalizeAp({ ssid: "G2-Alpha", key: "scripted-key" }, 0).auth).toBe("wpa2-psk");
  });

  test("every auth mode the card offers has the IDF number `env` takes", () => {
    for (const [name, mode] of Object.entries(wifi.AUTH_MODES)) {
      expect(Number.isInteger(mode), name).toBe(true);
      expect(mode, name).toBeGreaterThanOrEqual(0);
    }
    expect(new Set(Object.values(wifi.AUTH_MODES)).size).toBe(Object.keys(wifi.AUTH_MODES).length);
  });

  // `net_capture` reads always-on module state: there is no start and no stop.
  test("the pcap button saves the capture, and names it only when asked", () => {
    expect(wifi.captureArgs()).toEqual({ op: "save" });
    expect(wifi.captureArgs("dhcp")).toEqual({ op: "save", label: "dhcp" });
  });
});

describe("ble", () => {
  test("scan and connect round-trip through their arguments", () => {
    expect(ble.fromScanArgs(ble.scanArgs(1_500)).scanMs).toBe(1_500);
    const addr = "02:00:00:aa:bb:cc";
    expect(ble.fromConnectArgs(ble.connectArgs(addr)).connected).toBe(addr);
    expect(ble.fromConnectArgs(ble.disconnectArgs(addr)).connected).toBeNull();
  });

  test("a malformed address or scan length is refused with its field", () => {
    expect(() => ble.connectArgs("02:00:00:aa:bb")).toThrow(ble.BleFormError);
    expect(() => ble.scanArgs(0)).toThrow(ble.BleFormError);
    expect(() => ble.scanArgs(1.5)).toThrow(ble.BleFormError);
  });

  test("a peer outside the placeholder prefix is marked as not the emulator's", () => {
    expect(ble.isPlaceholderAddr("02:00:00:aa:bb:cc")).toBe(true);
    expect(ble.isPlaceholderAddr(NOT_A_PLACEHOLDER)).toBe(false);
  });

  test("the GATT operations carry what the scripted central takes", () => {
    expect(ble.gattArgs("discover")).toEqual({ op: "discover" });
    expect(ble.gattArgs("read", { uuid: "2a00" })).toEqual({ op: "read", uuid: "2a00" });
    expect(ble.gattArgs("subscribe", { handle: 42 })).toEqual({ op: "subscribe", handle: 42 });
    expect(ble.gattArgs("write", { uuid: "2a01" }, "DEADBEEF", false)).toEqual({
      op: "write",
      uuid: "2a01",
      value: "DEADBEEF",
      with_response: false,
    });
  });

  test("a write needs whole hex bytes and a target", () => {
    expect(() => ble.gattArgs("read", {})).toThrow(ble.BleFormError);
    expect(() => ble.gattArgs("write", { uuid: "2a01" }, "DEA")).toThrow(ble.BleFormError);
    expect(() => ble.gattArgs("write", { uuid: "2a01" }, "")).toThrow(ble.BleFormError);
  });

  test("a scan result becomes the peer list, and a malformed row is dropped", () => {
    const peers = ble.peersFromResult({
      found: [
        { addr: "02:00:00:78:4d:22", name: "Passport Keys", pdu: "ADV_IND", connectable: true },
        { name: "no address" },
      ],
    });
    expect(peers).toEqual([
      {
        addr: "02:00:00:78:4d:22",
        name: "Passport Keys",
        pdu: "ADV_IND",
        connectable: true,
        heard: true,
      },
    ]);
    // The air models no path loss, so no RSSI is reported (`ble.ts`).
    expect(peers[0]).not.toHaveProperty("rssi");
    expect(ble.peersFromResult(null)).toEqual([]);
  });

  test("a discover result becomes the GATT tree, with the CCCD under its characteristic", () => {
    const tree = ble.treeFromResult({
      tree: [
        {
          kind: "service",
          uuid: "12D4FA08",
          handle: 16,
          children: [
            {
              kind: "characteristic",
              uuid: "12D4FA09",
              handle: 18,
              properties: "notify",
              children: [{ kind: "descriptor", uuid: "2902", handle: 19 }],
            },
            { kind: "nonsense", uuid: "dropped", handle: 20 },
          ],
        },
      ],
    });
    expect(ble.flattenTree(tree).map((row) => row.node.uuid)).toEqual([
      "12D4FA08",
      "12D4FA09",
      "2902",
    ]);
    expect(ble.characteristics(tree).map((node) => node.uuid)).toEqual(["12D4FA09"]);
  });

  test("a notifications result carries the text the card shows and its new count", () => {
    const result = {
      new: 1,
      notifications: [
        { handle: 18, uuid: "12D4FA09", indication: false, text: '{"t":"pong"}', value: "7b" },
      ],
    };
    expect(ble.notificationsFromResult(result)[0]?.text).toBe('{"t":"pong"}');
    expect(ble.newCount(result)).toBe(1);
    expect(ble.newCount({})).toBe(0);
  });

  test("subscribe, write and poll spell the `ble_gatt` operations they name", () => {
    expect(ble.subscribeArgs("12D4FA09")).toEqual({ op: "subscribe", uuid: "12D4FA09" });
    expect(ble.writeTextArgs("12D4FA0A", '{"cmd":"ping"}\n')).toEqual({
      op: "write",
      uuid: "12D4FA0A",
      text: '{"cmd":"ping"}\n',
      with_response: true,
    });
    expect(ble.notificationsArgs("12D4FA09", 250)).toEqual({
      op: "notifications",
      uuid: "12D4FA09",
      settle_ms: 250,
    });
    expect(ble.notificationsArgs(null)).toEqual({ op: "notifications" });
    expect(() => ble.subscribeArgs("")).toThrow(ble.BleFormError);
    expect(() => ble.writeTextArgs("", "hi")).toThrow(ble.BleFormError);
    expect(() => ble.writeTextArgs("12D4FA0A", "")).toThrow(ble.BleFormError);
  });

  test("a disconnect clears what the connection held", () => {
    const connected: ble.BleState = {
      ...ble.DEFAULT_BLE,
      connected: "02:00:00:aa:bb:cc",
      subscribed: "12D4FA09",
      notifications: [{ handle: 18, uuid: null, indication: false, text: "x", value: "78" }],
    };
    const after = ble.fromConnectArgs(ble.disconnectArgs("02:00:00:aa:bb:cc"), connected);
    expect(after.connected).toBeNull();
    expect(after.subscribed).toBeNull();
    expect(after.notifications).toEqual([]);
  });

  test("the tree flattens with the depth the list indents by", () => {
    const tree: ble.GattNode[] = [
      {
        uuid: "180a",
        handle: 1,
        kind: "service",
        children: [{ uuid: "2a29", handle: 3, kind: "characteristic", properties: "read" }],
      },
    ];
    expect(ble.flattenTree(tree).map((row) => [row.node.uuid, row.depth])).toEqual([
      ["180a", 0],
      ["2a29", 1],
    ]);
  });
});

describe("audio", () => {
  const states: audio.AudioState[] = [
    audio.DEFAULT_AUDIO,
    { ...audio.DEFAULT_AUDIO, source: "tone", toneHz: 440, amplitude: 1_000 },
    { ...audio.DEFAULT_AUDIO, source: "file", fileName: "speech.wav" },
    { ...audio.DEFAULT_AUDIO, source: "live" },
  ];

  test("every state round-trips through its `mic_set` arguments", () => {
    for (const state of states) {
      expect(audio.fromArgs(audio.toArgs(state), state)).toEqual(state);
    }
  });

  test("a tone above the guest's Nyquist point is refused", () => {
    expect(() =>
      audio.toArgs({ ...audio.DEFAULT_AUDIO, source: "tone", toneHz: 12_000 }),
    ).toThrow(audio.AudioFormError);
    // The same frequency is fine when the source is not a tone: nothing generates it.
    expect(() =>
      audio.toArgs({ ...audio.DEFAULT_AUDIO, source: "file", fileName: "a.wav", toneHz: 12_000 }),
    ).not.toThrow();
  });

  test("an amplitude outside a 16-bit peak is refused", () => {
    expect(() =>
      audio.toArgs({ ...audio.DEFAULT_AUDIO, source: "tone", amplitude: 40_000 }),
    ).toThrow(audio.AudioFormError);
  });

  test("a file source without a name is refused before it reaches the command", () => {
    expect(() => audio.toArgs({ ...audio.DEFAULT_AUDIO, source: "file", fileName: "  " })).toThrow(
      audio.AudioFormError,
    );
  });

  test("each source sends only the fields `mic_set` reads for it", () => {
    expect(audio.toArgs({ ...audio.DEFAULT_AUDIO, source: "silence" })).toEqual({ kind: "silence" });
    expect(audio.toArgs({ ...audio.DEFAULT_AUDIO, source: "tone" })).toEqual({
      kind: "tone",
      hz: 1_000,
      amplitude: audio.MINUS_6_DBFS,
    });
    expect(audio.toArgs({ ...audio.DEFAULT_AUDIO, source: "file", fileName: " a.wav " })).toEqual({
      kind: "file",
      name: "a.wav",
    });
  });

  test("the mic source goes through `mic_set`, so the audio caps gate applies to it", () => {
    // `mic_set` is in the opt-in `audio` group and `env` in core; sending the source through `env`
    // would accept a mic change on a daemon that refuses `audio_capture`.
    expect(COMMAND_GROUP.mic_set).toBe("audio");
    expect(Object.keys(audio.toArgs(audio.DEFAULT_AUDIO))).toEqual(["kind"]);
  });

  test("a capture round-trips through `audio_capture` and refuses an impossible length", () => {
    const state = { ...audio.DEFAULT_AUDIO, captureMs: 250, captureMode: "analog" as const };
    expect(audio.captureArgs(state)).toEqual({ duration_ms: 250, mode: "analog" });
    expect(audio.fromCaptureArgs(audio.captureArgs(state))).toEqual(state);
    expect(() => audio.captureArgs({ ...state, captureMs: 0 })).toThrow(audio.AudioFormError);
    expect(() => audio.captureArgs({ ...state, captureMs: 600_001 })).toThrow(audio.AudioFormError);
  });

  test("a capture result is described by its fundamental and peak, never guessed", () => {
    expect(
      audio.describeCapture({
        analysis: { channel: 0, fundamental_hz: 440, peak: 16_422, rms: 11_612 },
        dropped_samples: 0,
      }),
    ).toBe("440 Hz, peak 16422");
    expect(
      audio.describeCapture({
        analysis: { channel: 0, fundamental_hz: null, peak: 3, rms: 1 },
        dropped_samples: 12,
      }),
    ).toBe("no fundamental, peak 3, 12 samples dropped");
    expect(audio.describeCapture({ analysis: null })).toBe("no samples captured");
    expect(audio.describeCapture("not an object")).toBe("no samples captured");
  });

  test("`live` is marked browser-only", () => {
    expect(audio.isBrowserOnly("live")).toBe(true);
    expect(audio.isBrowserOnly("tone")).toBe(false);
    expect(audio.MIC_SOURCES.find((s) => s.id === "live")?.note).toContain("browser-only");
  });

  test("the meter is an RMS level, so speech does not read as silence", () => {
    expect(audio.meterLevel(new Int16Array(0))).toBe(0);
    expect(audio.meterLevel(new Int16Array([0, 0, 0]))).toBe(0);
    const full = new Int16Array([32_767, -32_768, 32_767, -32_768]);
    expect(audio.meterLevel(full)).toBeGreaterThan(0.99);
    // A signal that is mostly zero with rare peaks reads low, which a peak meter would not.
    const sparse = new Int16Array(100);
    sparse[0] = 32_767;
    expect(audio.meterLevel(sparse)).toBeLessThan(0.2);
  });

  test("digital silence reads as -inf rather than 0 dBFS", () => {
    expect(audio.meterDbfs(0)).toBe("-inf");
    expect(audio.meterDbfs(1)).toBe("0");
  });
});

describe("every panel's arguments reach the registry through the one client", () => {
  test("a battery change is journaled as the `env` call an agent would make", async () => {
    const journal = new UiJournal();
    const sent: string[] = [];
    const client = new CommandClient(
      (request) => {
        sent.push(request);
        return Promise.resolve({ ok: '{"json":{},"text":""}' });
      },
      { journal },
    );
    await client.call("env", battery.changeArgs({ kind: "soc", soc: 20 }));
    expect(journal.last()?.command).toBe("env");
    expect(JSON.parse(sent[0] ?? "{}")).toEqual({ cmd: "env", args: { battery: { soc: 20 } } });
  });
});

describe("ble radio state", () => {
  const refusal = (detail: unknown, code = "E_STATE") => new CommandError("ble_scan", { code, message: "refused", detail: detail as never });

  test("a refusal names the radio's state from its detail, never from its English", () => {
    expect(ble.radioFromRefusal(refusal({ ble: "not_started", binding: "bound" }))).toEqual({ kind: "not_started" });
    expect(ble.radioFromRefusal(refusal({ ble: "stopped" }))).toEqual({ kind: "stopped" });
    expect(ble.radioFromRefusal(refusal({ ble: "not_bound", binding: "unsupported image", elf: false }))).toEqual({
      kind: "not_bound",
      binding: "unsupported image",
      elf: false,
    });
    // A reason is not a state, another code is not the radio's, and a plain error is neither.
    expect(ble.radioFromRefusal(refusal({ ble: "non_connectable", pdu: "ADV_SCAN_IND" }))).toBeNull();
    expect(ble.radioFromRefusal(refusal({ ble: "not_started" }, "E_USAGE"))).toBeNull();
    expect(ble.radioFromRefusal(new Error("no bound BLE module"))).toBeNull();
  });

  test("a refusal's reason carries the PDU the firmware advertises", () => {
    expect(ble.whyOf(refusal({ ble: "non_connectable", pdu: "ADV_SCAN_IND", addr: "02:00:00:78:4D:22" }))).toEqual({
      why: "non_connectable",
      addr: "02:00:00:78:4D:22",
      pdu: "ADV_SCAN_IND",
    });
    for (const why of ["not_connected", "not_advertising", "peer_not_seen", "no_answer"] as const) {
      expect(ble.whyOf(refusal({ ble: why }))).toEqual({ why });
    }
    expect(ble.whyOf(refusal({ ble: "stopped" }))).toBeNull();
  });

  test("an answer's radio object is the state line", () => {
    const adv = { addr: "02:00:00:78:4D:22", pdu: "ADV_SCAN_IND", connectable: false, name: "FoloPassport", random: false };
    expect(ble.radioFromResult({ radio: { state: "advertising", advertising: adv } })).toEqual({
      kind: "advertising",
      adv: { addr: adv.addr, pdu: adv.pdu, connectable: false, name: "FoloPassport" },
    });
    expect(ble.radioFromResult({ radio: { state: "idle", advertising: null } })).toEqual({ kind: "idle" });
    expect(ble.radioFromResult({ radio: { state: "connected", advertising: null } })).toEqual({ kind: "connected", adv: null });
    expect(ble.radioFromResult({})).toBeNull();
  });

  test("a scan lists what it heard and says when the air was silent", () => {
    const json = {
      outcome: "heard",
      heard: 1,
      adv_events: 18,
      found: [
        { addr: "02:00:00:78:4D:22", name: "FoloPassport", pdu: "ADV_SCAN_IND", connectable: false, heard: true },
        { addr: "02:00:00:78:4D:23", name: null, pdu: "ADV_IND", connectable: true, heard: false },
      ],
    };
    const peers = ble.peersFromResult(json);
    expect(ble.heardPeers(peers).map((peer) => peer.addr)).toEqual(["02:00:00:78:4D:22"]);
    expect(peers[1]?.name).toBe(ble.NO_NAME);
    expect(ble.scanOutcomeFromResult(json)).toEqual({ outcome: "heard", heard: 1, advEvents: 18 });
    expect(ble.scanOutcomeFromResult({ outcome: "silent", heard: 0, adv_events: 0 })?.outcome).toBe("silent");
    expect(ble.scanOutcomeFromResult({})).toBeNull();
    // An answer from before `heard` existed lists every advertiser.
    expect(ble.heardPeers(ble.peersFromResult({ found: [{ addr: "02:00:00:00:00:01" }] }))).toHaveLength(1);
  });

  test("the state read scans nothing", () => {
    expect(ble.READ_ARGS).toEqual({ duration_ms: 0 });
  });
});
