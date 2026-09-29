// The page's remembered choices. Every `localStorage` access is wrapped: the property throws when
// site data is blocked, and `getItem`/`setItem` throw in private windows and over quota, none of
// which may stop the emulator. A URL parameter (`?mode=`, `?lang=`, `?theme=`, `?zoom=`) overrides
// the stored choice for one load and is not written back.

import { detectLocale, isLocale, type Locale } from "./i18n";

export type Mode = "simple" | "advanced";
export const MODES: readonly Mode[] = ["simple", "advanced"];
export const DEFAULT_MODE: Mode = "simple";

export type ThemeChoice = "system" | "light" | "dark";
export const THEME_CHOICES: readonly ThemeChoice[] = ["system", "light", "dark"];

/** A percentage of the device's nominal size, or `fit`: the largest that fits column and height. */
export type Zoom = "fit" | "100" | "140" | "180";
export const ZOOMS: readonly Zoom[] = ["fit", "100", "140", "180"];
export const DEFAULT_ZOOM: Zoom = "fit";

export const PREF_KEYS = {
  mode: "passportsim.mode",
  locale: "passportsim.locale",
  theme: "passportsim.theme",
  zoom: "passportsim.zoom",
  follow: "passportsim.follow",
} as const;

export interface StorageLike {
  getItem(key: string): string | null;
  setItem(key: string, value: string): void;
}

export interface SafeStorage {
  read(key: string): string | null;
  write(key: string, value: string): boolean;
}

/** Wraps a storage getter so neither it nor the storage can throw; `window.localStorage` itself does. */
export function safeStorage(open: () => StorageLike | null | undefined): SafeStorage {
  return {
    read(key) {
      try {
        return open()?.getItem(key) ?? null;
      } catch {
        return null;
      }
    },
    write(key, value) {
      try {
        const storage = open();
        if (!storage) {
          return false;
        }
        storage.setItem(key, value);
        return true;
      } catch {
        return false;
      }
    },
  };
}

export function isMode(value: unknown): value is Mode {
  return value === "simple" || value === "advanced";
}

export function isZoom(value: unknown): value is Zoom {
  return typeof value === "string" && (ZOOMS as readonly string[]).includes(value);
}

export function isThemeChoice(value: unknown): value is ThemeChoice {
  return value === "system" || value === "light" || value === "dark";
}

function param(search: string, name: string): string | null {
  try {
    return new URLSearchParams(search).get(name);
  } catch {
    return null;
  }
}

export function initialMode(search: string, storage: SafeStorage): Mode {
  const fromUrl = param(search, "mode");
  if (isMode(fromUrl)) {
    return fromUrl;
  }
  const stored = storage.read(PREF_KEYS.mode);
  return isMode(stored) ? stored : DEFAULT_MODE;
}

/** The URL's language, else the stored one, else the system's. Only an explicit switch stores one. */
export function initialLocale(search: string, storage: SafeStorage, systemLocale: string): Locale {
  const fromUrl = param(search, "lang");
  if (isLocale(fromUrl)) {
    return fromUrl;
  }
  const stored = storage.read(PREF_KEYS.locale);
  return isLocale(stored) ? stored : detectLocale(systemLocale);
}

export function initialTheme(search: string, storage: SafeStorage): ThemeChoice {
  const fromUrl = param(search, "theme");
  if (isThemeChoice(fromUrl)) {
    return fromUrl;
  }
  const stored = storage.read(PREF_KEYS.theme);
  return isThemeChoice(stored) ? stored : "system";
}

export function initialZoom(search: string, storage: SafeStorage): Zoom {
  const fromUrl = param(search, "zoom");
  if (isZoom(fromUrl)) {
    return fromUrl;
  }
  const stored = storage.read(PREF_KEYS.zoom);
  return isZoom(stored) ? stored : DEFAULT_ZOOM;
}

export function initialFollow(storage: SafeStorage): boolean {
  return storage.read(PREF_KEYS.follow) !== "false";
}

export function resolveTheme(choice: ThemeChoice, prefersDark: boolean): "light" | "dark" {
  return choice === "system" ? (prefersDark ? "dark" : "light") : choice;
}
