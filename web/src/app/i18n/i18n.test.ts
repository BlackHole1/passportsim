import { describe, expect, test } from "bun:test";
import { en } from "./en";
import { detectLocale, format, LOCALES, MESSAGES, placeholders, translator, type Messages } from ".";

// A missing key is a type error; if `Messages` stopped requiring every key, the directive below
// would be unused and the typecheck would fail on that instead.
const { "mode.label": _dropped, ...partial } = en;
// @ts-expect-error a locale without "mode.label" is not a `Messages`
const incomplete: Messages = partial;
void incomplete;

describe("every locale", () => {
  const keys = Object.keys(en).sort();

  for (const locale of LOCALES) {
    test(`${locale} has exactly the keys of en, none empty`, () => {
      const messages = MESSAGES[locale];
      expect(Object.keys(messages).sort()).toEqual(keys);
      for (const key of keys) {
        expect(messages[key as keyof Messages].trim().length).toBeGreaterThan(0);
      }
    });

    test(`${locale} keeps each message's placeholders`, () => {
      const messages = MESSAGES[locale];
      for (const key of keys as (keyof Messages)[]) {
        expect({ key, names: placeholders(messages[key]) }).toEqual({ key, names: placeholders(en[key]) });
      }
    });

    test(`${locale} uses none of the characters the project's text avoids`, () => {
      for (const value of Object.values(MESSAGES[locale])) {
        expect(value).not.toMatch(/[（）·]|——/);
      }
    });
  }

  test("the non-English dictionaries are translations, not copies", () => {
    for (const locale of LOCALES.filter((one) => one !== "en")) {
      expect(MESSAGES[locale]["firmware.drop"]).not.toBe(en["firmware.drop"]);
    }
  });
});

describe("lookup", () => {
  test("interpolates by name and leaves an unknown name as written", () => {
    expect(format("vt {vt} at {speed}", { vt: "1.000 s" })).toBe("vt 1.000 s at {speed}");
    expect(translator("en")("status.vt", { vt: "2.000 s" })).toBe("vt 2.000 s");
    expect(translator("zh-CN")("mode.simple")).toBe(MESSAGES["zh-CN"]["mode.simple"]);
  });
});

describe("detectLocale", () => {
  test("matches the system tag's primary language", () => {
    expect(detectLocale("fr-CA")).toBe("fr");
    expect(detectLocale("ja-JP")).toBe("ja");
    expect(detectLocale("en-GB")).toBe("en");
  });

  test("reads any Chinese as Simplified Chinese", () => {
    expect(detectLocale("zh-CN")).toBe("zh-CN");
    expect(detectLocale("zh-TW")).toBe("zh-CN");
    expect(detectLocale("zh-Hans-CN")).toBe("zh-CN");
    expect(detectLocale("zh_HK")).toBe("zh-CN");
    expect(detectLocale("ZH")).toBe("zh-CN");
  });

  test("falls back to English for a language the page does not carry, or none", () => {
    expect(detectLocale("")).toBe("en");
    expect(detectLocale("de-DE")).toBe("en");
    expect(detectLocale("la")).toBe("en");
  });
});
