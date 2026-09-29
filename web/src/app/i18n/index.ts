// The page's own translations, with no i18n library: lookup and `{name}` interpolation only. `en`
// is the source dictionary and its key set is the type every locale must satisfy. Firmware output,
// guest UI labels, core error texts and command names are data and are not translated.

import { en } from "./en";
import { fr } from "./fr";
import { ja } from "./ja";
import { zhCN } from "./zh-CN";

export type MessageKey = keyof typeof en;
export type Messages = { readonly [K in MessageKey]: string };

export const LOCALES = ["en", "zh-CN", "ja", "fr"] as const;
export type Locale = (typeof LOCALES)[number];

export const MESSAGES: Readonly<Record<Locale, Messages>> = { en, "zh-CN": zhCN, ja, fr };

export const LOCALE_NAMES: Readonly<Record<Locale, string>> = {
  en: "English",
  "zh-CN": "简体中文",
  ja: "日本語",
  fr: "Français",
};

export function isLocale(value: unknown): value is Locale {
  return typeof value === "string" && (LOCALES as readonly string[]).includes(value);
}

/**
 * The locale for the system's language tag, else English; any Chinese variant reads Simplified
 * Chinese. The tag is the system's, not `navigator.languages[0]`, so a Chinese system whose
 * browser lists English first still opens in Chinese.
 */
export function detectLocale(systemLocale: string): Locale {
  const primary = systemLocale.trim().toLowerCase().split(/[-_]/)[0];
  switch (primary) {
    case "zh":
      return "zh-CN";
    case "ja":
      return "ja";
    case "fr":
      return "fr";
    default:
      return "en";
  }
}

export function systemLocale(): string {
  try {
    return Intl.DateTimeFormat().resolvedOptions().locale;
  } catch {
    return "";
  }
}

export type Params = Readonly<Record<string, string | number | bigint>>;

/** Replaces each `{name}` with its value; a name with no value is left as written. */
export function format(template: string, params?: Params): string {
  if (!params) {
    return template;
  }
  return template.replace(/\{([a-zA-Z0-9_]+)\}/g, (whole, name: string) =>
    name in params ? String(params[name]) : whole,
  );
}

export type Translate = (key: MessageKey, params?: Params) => string;

export function translator(locale: Locale): Translate {
  const messages = MESSAGES[locale];
  return (key, params) => format(messages[key], params);
}

export function placeholders(template: string): string[] {
  return [...template.matchAll(/\{([a-zA-Z0-9_]+)\}/g)].map((match) => match[1] ?? "").sort();
}
