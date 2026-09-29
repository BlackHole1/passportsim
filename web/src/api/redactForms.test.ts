import { describe, expect, test } from "bun:test";
import { SECRET, redactCall, sessionSecrets } from "./redact";

function maskedWrite(setup: Parameters<typeof sessionSecrets>[0], text: string): string {
  const out = redactCall("serial", { op: "write", text }, sessionSecrets(setup)).args as { text: string };
  return out.text;
}

const utf8Hex = (text: string) => Buffer.from(text, "utf8").toString("hex");

describe("by-value forms (finding 6)", () => {
  const wifi = [{ command: "nfc_tag" as const, args: { ndef: [{ type: "wifi", ssid: "lab", auth: "wpa2", encr: "aes", key: "correct-horse" }] } }];

  test("a Wi-Fi key is masked as text, lower and upper hex, colon hex and base64", () => {
    const hex = utf8Hex("correct-horse");
    const forms = [
      "correct-horse",
      hex,
      hex.toUpperCase(),
      hex.match(/../g)?.join(":") ?? "",
      Buffer.from("correct-horse").toString("base64"),
      Buffer.from("correct-horse").toString("base64").replace(/=+$/, ""),
    ];
    for (const form of forms) {
      expect(maskedWrite(wifi, `k=${form};`)).toBe(`k=${SECRET};`);
    }
  });

  test("an NFC UID is masked in any hex case and with colon separators, and as base64", () => {
    const uid = [{ command: "nfc_tag" as const, args: { uid: "04A1B2C3D4E5F6" } }];
    for (const form of ["04a1b2c3d4e5f6", "04:a1:b2:c3:d4:e5:f6", "04-A1-B2-C3-D4-E5-F6", Buffer.from("04a1b2c3d4e5f6", "hex").toString("base64")]) {
      expect(maskedWrite(uid, `uid ${form} end`)).toBe(`uid ${SECRET} end`);
    }
  });

  test("a PWD_AUTH password is masked whatever its case", () => {
    const tap = [{ command: "nfc_tap" as const, args: { ops: [{ op: "raw", frames: ["1b a1b2c3d4"] }] } }];
    expect(maskedWrite(tap, "pwd A1B2C3D4")).toBe(`pwd ${SECRET}`);
  });
});
