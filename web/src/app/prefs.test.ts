import { describe, expect, test } from "bun:test";
import {
  initialLocale,
  initialMode,
  initialZoom,
  initialTheme,
  PREF_KEYS,
  resolveTheme,
  safeStorage,
  type StorageLike,
} from "./prefs";

function memory(entries: Record<string, string> = {}): StorageLike & { readonly map: Map<string, string> } {
  const map = new Map(Object.entries(entries));
  return {
    map,
    getItem: (key) => map.get(key) ?? null,
    setItem: (key, value) => {
      map.set(key, value);
    },
  };
}

/** A storage whose every method throws, as a private window over quota does. */
const throwing: StorageLike = {
  getItem: () => {
    throw new Error("SecurityError");
  },
  setItem: () => {
    throw new Error("QuotaExceededError");
  },
};

describe("safeStorage", () => {
  test("reads and writes through", () => {
    const backing = memory();
    const storage = safeStorage(() => backing);
    expect(storage.write(PREF_KEYS.mode, "advanced")).toBe(true);
    expect(storage.read(PREF_KEYS.mode)).toBe("advanced");
  });

  test("a throwing getter, a throwing storage and no storage all read null and write false", () => {
    for (const open of [
      () => {
        throw new Error("SecurityError: localStorage is blocked");
      },
      () => throwing,
      () => null,
    ]) {
      const storage = safeStorage(open);
      expect(storage.read(PREF_KEYS.mode)).toBeNull();
      expect(storage.write(PREF_KEYS.mode, "advanced")).toBe(false);
    }
  });
});

describe("the first choices of a load", () => {
  test("a first visit is simple mode, the system's language and the system theme", () => {
    const storage = safeStorage(() => memory());
    expect(initialMode("", storage)).toBe("simple");
    expect(initialLocale("", storage, "ja-JP")).toBe("ja");
    expect(initialTheme("", storage)).toBe("system");
  });

  test("a stored choice wins over the default", () => {
    const storage = safeStorage(() =>
      memory({ [PREF_KEYS.mode]: "advanced", [PREF_KEYS.locale]: "fr", [PREF_KEYS.theme]: "dark" }),
    );
    expect(initialMode("", storage)).toBe("advanced");
    expect(initialLocale("", storage, "ja-JP")).toBe("fr");
    expect(initialTheme("", storage)).toBe("dark");
  });

  test("the URL wins over storage, and nonsense in either is ignored", () => {
    const storage = safeStorage(() => memory({ [PREF_KEYS.mode]: "advanced", [PREF_KEYS.locale]: "klingon" }));
    expect(initialMode("?mode=simple", storage)).toBe("simple");
    expect(initialMode("?mode=expert", storage)).toBe("advanced");
    expect(initialLocale("?lang=zh-CN", storage, "en")).toBe("zh-CN");
    expect(initialLocale("", storage, "en")).toBe("en");
    expect(initialTheme("?theme=light", storage)).toBe("light");
  });

  test("storage that throws falls back to the defaults", () => {
    const storage = safeStorage(() => throwing);
    expect(initialMode("", storage)).toBe("simple");
    expect(initialLocale("", storage, "fr-FR")).toBe("fr");
    expect(initialTheme("", storage)).toBe("system");
  });

  test("the owner's Chrome: a zh-CN system whose browser lists English first opens in Chinese", () => {
    // `navigator.languages` is not an input.
    const storage = safeStorage(() => memory());
    expect(initialLocale("", storage, "zh-CN")).toBe("zh-CN");
    expect(initialLocale("", storage, "de-DE")).toBe("en");
    expect(initialLocale("", safeStorage(() => memory({ [PREF_KEYS.locale]: "ja" })), "zh-CN")).toBe("ja");
    expect(initialLocale("?lang=fr", safeStorage(() => memory({ [PREF_KEYS.locale]: "ja" })), "zh-CN")).toBe("fr");
  });

  test("the zoom: the URL's, else the stored one, else fit; the retired size and calibration are ignored", () => {
    const empty = safeStorage(() => memory({}));
    expect(initialZoom("", empty)).toBe("fit");
    const stored = safeStorage(() => memory({ [PREF_KEYS.zoom]: "140" }));
    expect(initialZoom("", stored)).toBe("140");
    expect(initialZoom("?zoom=180", stored)).toBe("180");
    expect(initialZoom("?zoom=150", stored)).toBe("140");
    // An old link or an old stored choice of the true-size view opens at the default zoom.
    const old = safeStorage(() => memory({ "passportsim.size": "real", "passportsim.calibration": "1.08" }));
    expect(initialZoom("?size=real", old)).toBe("fit");
    expect(initialZoom("", safeStorage(() => throwing))).toBe("fit");
  });

  test("system follows the operating system; light and dark do not", () => {
    expect(resolveTheme("system", true)).toBe("dark");
    expect(resolveTheme("system", false)).toBe("light");
    expect(resolveTheme("light", true)).toBe("light");
    expect(resolveTheme("dark", false)).toBe("dark");
  });
});
