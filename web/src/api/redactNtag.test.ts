import { describe, expect, test } from "bun:test";
import { SECRET, redactCall, sessionSecrets } from "./redact";

function maskedWrite(setup: Parameters<typeof sessionSecrets>[0], text: string): string {
  const out = redactCall("serial", { op: "write", text }, sessionSecrets(setup)).args as { text: string };
  return out.text;
}

describe("NTAG213 PWD and PACK pages (finding 7)", () => {
  test("a WRITE to page 0x2B (PWD) or 0x2C (PACK) is a secret frame, and its data is masked by value", () => {
    const args = { ops: [{ op: "raw", frames: ["a2 2b 11223344", "a2 2c 5566 0000", "30 04"] }] };
    const secrets = sessionSecrets([{ command: "nfc_tap", args }]);
    expect(redactCall("nfc_tap", args, secrets).args).toEqual({ ops: [{ op: "raw", frames: [SECRET, SECRET, "30 04"] }] });
    // PACK is two bytes: NFC UID, PWD and PACK have no 6-character floor.
    expect(redactCall("serial", { op: "write", text: "pwd 11223344 pack 5566" }, secrets).args).toEqual({
      op: "write",
      text: `pwd ${SECRET} pack ${SECRET}`,
    });
  });

  test("a COMPAT_WRITE to the PWD page masks its second frame, which carries the data", () => {
    const args = { ops: [{ op: "raw", frames: ["a0 2b", "99887766 000000000000000000000000"] }] };
    const secrets = sessionSecrets([{ command: "nfc_tap", args }]);
    expect(redactCall("nfc_tap", args, secrets).args).toEqual({ ops: [{ op: "raw", frames: ["a0 2b", SECRET] }] });
    expect(redactCall("serial", { op: "write", text: "99887766" }, secrets).args).toEqual({ op: "write", text: SECRET });
  });

  test("a short Wi-Fi key keeps the 6-character floor for credential values", () => {
    const wifi = [{ command: "nfc_tag" as const, args: { ndef: [{ type: "wifi", ssid: "lab", auth: "wpa2", encr: "aes", key: "abc" }] } }];
    expect(maskedWrite(wifi, "abc")).toBe("abc");
  });
});
