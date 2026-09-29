import { describe, expect, test } from "bun:test";
import {
  DEFAULT_DWELL_MS,
  LOCK_CONFIRMATION,
  LOCK_NEEDS_CONFIRMATION,
  TEARING_DWELL_MS,
  TapFormError,
  droppedTap,
  ndefRecord,
  tapArgs,
  tapOps,
} from "./tap";

describe("the tap presets", () => {
  test("read NDEF is one readNdef op at the default dwell", () => {
    expect(tapArgs({ preset: "read" })).toEqual({
      ops: [{ op: "readNdef" }],
      dwell_ms: DEFAULT_DWELL_MS,
    });
  });

  test("write URI produces the NDEF URI record", () => {
    expect(tapArgs({ preset: "write-uri", uri: "https://example.com/p" })).toEqual({
      ops: [{ op: "writeNdef", ndef: [{ type: "uri", uri: "https://example.com/p" }] }],
      dwell_ms: DEFAULT_DWELL_MS,
    });
  });

  test("write Text carries an optional language", () => {
    expect(ndefRecord({ preset: "write-text", text: "hello" })).toEqual({
      type: "text",
      text: "hello",
    });
    expect(ndefRecord({ preset: "write-text", text: "hello", lang: "en" })).toEqual({
      type: "text",
      text: "hello",
      lang: "en",
    });
  });

  test("write Wi-Fi defaults auth and encryption to WPA2-Personal and AES", () => {
    expect(ndefRecord({ preset: "write-wifi", ssid: "TestAP", key: "password1" })).toEqual({
      type: "wifi",
      ssid: "TestAP",
      auth: "wpa2-personal",
      encr: "aes",
      key: "password1",
    });
  });

  test("the raw T2T console sends the frames as hex", () => {
    expect(tapOps({ preset: "raw", frames: ["60", "3004", "A204DEADBEEF"] })).toEqual([
      { op: "raw", frames: ["60", "3004", "A204DEADBEEF"] },
    ]);
  });

  test("tearing is the write preset at a dwell too short to finish", () => {
    const args = tapArgs({ preset: "tear", uri: "https://example.com/p" });
    expect(args.dwell_ms).toBe(TEARING_DWELL_MS);
    expect(args.ops[0]).toMatchObject({ op: "writeNdef" });
  });
});

describe("form validation", () => {
  test("an empty raw frame list is refused with the field that is wrong", () => {
    expect(() => tapOps({ preset: "raw", frames: [] })).toThrow(TapFormError);
    try {
      tapOps({ preset: "raw", frames: ["  "] });
    } catch (error) {
      expect((error as TapFormError).field).toBe("frames");
    }
  });

  test("a frame that is not whole hex bytes is refused", () => {
    expect(() => tapOps({ preset: "raw", frames: ["30 04"] })).toThrow(TapFormError);
    expect(() => tapOps({ preset: "raw", frames: ["A2B"] })).toThrow(TapFormError);
    expect(() => tapOps({ preset: "raw", frames: ["zz"] })).toThrow(TapFormError);
  });

  test("a missing required field names itself", () => {
    for (const [form, field] of [
      [{ preset: "write-uri" as const }, "uri"],
      [{ preset: "write-text" as const }, "text"],
      [{ preset: "write-wifi" as const, key: "k" }, "ssid"],
      [{ preset: "write-wifi" as const, ssid: "s" }, "key"],
    ] as const) {
      try {
        ndefRecord(form);
        throw new Error(`expected ${field} to be refused`);
      } catch (error) {
        expect(error).toBeInstanceOf(TapFormError);
        expect((error as TapFormError).field).toBe(field);
      }
    }
  });

  test("a dwell that is not a whole positive millisecond count is refused", () => {
    expect(() => tapArgs({ preset: "read", dwellMs: 0 })).toThrow(TapFormError);
    expect(() => tapArgs({ preset: "read", dwellMs: -5 })).toThrow(TapFormError);
    expect(() => tapArgs({ preset: "read", dwellMs: 1.5 })).toThrow(TapFormError);
  });

  test("an explicit dwell overrides the preset's default, including for tearing", () => {
    expect(tapArgs({ preset: "read", dwellMs: 900 }).dwell_ms).toBe(900);
    expect(tapArgs({ preset: "tear", uri: "u:1", dwellMs: 5 }).dwell_ms).toBe(5);
  });
});

describe("dropping on the NFC zone", () => {
  test("a dropped URL becomes a URI record", () => {
    expect(droppedTap("  https://example.com/p  ")).toEqual({
      ops: [{ op: "writeNdef", ndef: [{ type: "uri", uri: "https://example.com/p" }] }],
      dwell_ms: DEFAULT_DWELL_MS,
    });
  });

  test("dropped prose becomes a text record", () => {
    expect(droppedTap("hello passport")).toEqual({
      ops: [{ op: "writeNdef", ndef: [{ type: "text", text: "hello passport" }] }],
      dwell_ms: DEFAULT_DWELL_MS,
    });
  });

  test("a drop never writes a Wi-Fi record, which needs four fields", () => {
    const args = droppedTap("wifi:TestAP");
    expect(args.ops[0]).toMatchObject({ op: "writeNdef" });
    const op = args.ops[0];
    if (op && op.op === "writeNdef") {
      expect(op.ndef[0]?.type).not.toBe("wifi");
    }
  });
});

describe("locking", () => {
  test("locking is behind a confirmation, because the OTP bits are irreversible", () => {
    expect(LOCK_NEEDS_CONFIRMATION).toBe(true);
    expect(LOCK_CONFIRMATION).toContain("cannot be undone");
  });
});
