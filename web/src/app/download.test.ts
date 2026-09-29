import { describe, expect, test } from "bun:test";
import { downloadAmount, downloading, downloadPercent, downloadText, formatBytes } from "./download";
import { translator } from "./i18n";

const en = translator("en");

describe("the figures", () => {
  test("sizes are decimal, as a browser's download list shows them", () => {
    expect(formatBytes(512)).toBe("512 B");
    expect(formatBytes(7_500)).toBe("7.5 kB");
    expect(formatBytes(24_700_000)).toBe("24.7 MB");
  });

  test("the percent rounds down, so 100 means every byte", () => {
    expect(downloadPercent(7_500_000, 24_700_000)).toBe(30);
    expect(downloadPercent(24_699_999, 24_700_000)).toBe(99);
    expect(downloadPercent(24_700_000, 24_700_000)).toBe(100);
    expect(downloadPercent(0, 0)).toBe(100);
    expect(downloadPercent(5, null)).toBeNull();
    expect(downloadPercent(11, 10)).toBeNull();
  });

  test("both figures of a pair share the total's unit", () => {
    expect(downloadAmount(en, { received: 7_500_000, total: 24_700_000 })).toBe("7.5 / 24.7 MB (30%)");
    expect(downloadAmount(en, { received: 900_000, total: 24_700_000 })).toBe("0.9 / 24.7 MB (3%)");
  });

  test("without a total, the bytes alone; before the first byte, nothing", () => {
    expect(downloadAmount(en, { received: 3_200_000, total: null })).toBe("3.2 MB");
    expect(downloadAmount(en, { received: 0, total: null })).toBe("");
  });
});

describe("the sentence", () => {
  test("says what is downloading and how far it got", () => {
    expect(downloadText(en, { what: "firmware", received: 7_500_000, total: 24_700_000, done: false })).toBe(
      "Downloading the demo firmware 7.5 / 24.7 MB (30%)",
    );
    expect(downloadText(en, { what: "core", received: 0, total: null, done: false })).toBe("Downloading the emulator core");
    expect(downloadText(en, { what: "core", received: 1_000_000, total: null, done: false })).toBe(
      "Downloading the emulator core 1.0 MB",
    );
  });

  test("then that it finished, or why it failed", () => {
    expect(downloadText(en, { what: "core", received: 7_036_068, total: 7_036_068, done: true })).toBe(
      "Downloaded the emulator core (7.0 MB)",
    );
    expect(downloadText(en, { what: "firmware", received: 10, total: 100, done: true, error: "network error" })).toBe(
      "Could not download the demo firmware: network error",
    );
    expect(downloading({ what: "core", received: 1, total: 2, done: false })).toBe(true);
    expect(downloading({ what: "core", received: 2, total: 2, done: true })).toBe(false);
    expect(downloading(null)).toBe(false);
  });

  test("is worded in each of the page's languages", () => {
    const progress = { what: "firmware", received: 7_500_000, total: 24_700_000, done: false } as const;
    expect(downloadText(translator("zh-CN"), progress)).toBe("正在下载演示固件 7.5 / 24.7 MB (30%)");
    expect(downloadText(translator("ja"), progress)).toBe("デモファームウェアをダウンロードしています 7.5 / 24.7 MB (30%)");
    expect(downloadText(translator("fr"), progress)).toBe("Téléchargement du firmware de démo 7.5 / 24.7 MB (30 %)");
  });
});
