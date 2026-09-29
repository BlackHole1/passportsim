// The header: brand, mode switch, language and theme. The `Emulator` badge lives in the static
// `index.html`, so a screenshot is marked before the bundle runs; the header leaves room for it.

import { LanguagesIcon, MonitorIcon, MoonIcon, SunIcon } from "lucide-react";
import { useLayoutEffect, useRef } from "react";
import { Button } from "../../ui/button";
import { Menu, MenuGroup, MenuGroupLabel, MenuPopup, MenuRadioGroup, MenuRadioItem, MenuTrigger } from "../../ui/menu";
import { Tabs, TabsList, TabsTab } from "../../ui/tabs";
import { LOCALE_NAMES, LOCALES, isLocale } from "../i18n";
import { isMode, isThemeChoice, THEME_CHOICES, type ThemeChoice } from "../prefs";
import { usePage, useStore, useT } from "./hooks";

function Logo() {
  return (
    <svg aria-hidden="true" className="size-6 shrink-0 text-foreground" fill="none" viewBox="0 0 24 24">
      <rect height="20" rx="3.5" stroke="currentColor" strokeWidth="1.75" width="14" x="5" y="2" />
      <rect fill="currentColor" height="8" rx="1" width="8" x="8" y="6" />
      <rect fill="currentColor" height="1.75" rx="0.875" width="4" x="10" y="17" />
    </svg>
  );
}

const THEME_ICON: Record<ThemeChoice, typeof SunIcon> = {
  system: MonitorIcon,
  light: SunIcon,
  dark: MoonIcon,
};

export function Header() {
  const page = usePage();
  const t = useT();
  const prefs = useStore(page.prefs);
  const brand = useRef<HTMLDivElement>(null);

  // Where the brand ends is where the static badge sits (`styles.css` `.emulator-badge`).
  useLayoutEffect(() => {
    const node = brand.current;
    if (!node) {
      return;
    }
    const place = () => {
      const right = node.getBoundingClientRect().right;
      document.documentElement.style.setProperty("--badge-left", `${Math.round(right + 10)}px`);
    };
    place();
    window.addEventListener("resize", place);
    return () => {
      window.removeEventListener("resize", place);
    };
  }, [prefs.locale]);

  const ThemeIcon = THEME_ICON[prefs.theme];
  return (
    <header className="app-header sticky top-0 z-30 flex h-14 shrink-0 items-center gap-2 border-b bg-background/85 px-4 backdrop-blur-md sm:px-6">
      <div className="brand flex items-center gap-2.5" ref={brand}>
        <Logo />
        <span className="hidden font-semibold text-sm tracking-tight sm:inline">PassportSim</span>
      </div>
      {/* Room for the static badge, so nothing on the right ever runs under it. */}
      <span aria-hidden="true" className="w-22 shrink-0" />
      <div className="ms-auto flex items-center gap-1 sm:gap-2">
        <Tabs
          onValueChange={(value) => {
            if (isMode(value)) {
              page.actions.setMode(value);
            }
          }}
          value={prefs.mode}
        >
          <TabsList aria-label={t("mode.label")} size="sm">
            <TabsTab data-mode-switch="simple" value="simple">
              {t("mode.simple")}
            </TabsTab>
            <TabsTab data-mode-switch="advanced" value="advanced">
              {t("mode.advanced")}
            </TabsTab>
          </TabsList>
        </Tabs>
        <Menu>
          <MenuTrigger
            render={
              <Button aria-label={t("language.label")} data-menu="language" size="sm" title={t("language.label")} variant="ghost">
                <LanguagesIcon aria-hidden="true" />
                <span className="hidden md:inline">{LOCALE_NAMES[prefs.locale]}</span>
              </Button>
            }
          />
          <MenuPopup align="end">
            <MenuGroup>
              <MenuGroupLabel>{t("language.label")}</MenuGroupLabel>
              <MenuRadioGroup
                onValueChange={(value) => {
                  if (isLocale(value)) {
                    page.actions.setLocale(value);
                  }
                }}
                value={prefs.locale}
              >
                {LOCALES.map((locale) => (
                  <MenuRadioItem data-locale={locale} key={locale} lang={locale} value={locale}>
                    {LOCALE_NAMES[locale]}
                  </MenuRadioItem>
                ))}
              </MenuRadioGroup>
            </MenuGroup>
          </MenuPopup>
        </Menu>
        <Menu>
          <MenuTrigger
            render={
              <Button aria-label={t("theme.label")} data-menu="theme" size="icon-sm" title={t("theme.label")} variant="ghost">
                <ThemeIcon aria-hidden="true" />
              </Button>
            }
          />
          <MenuPopup align="end">
            <MenuGroup>
              <MenuGroupLabel>{t("theme.label")}</MenuGroupLabel>
              <MenuRadioGroup
                onValueChange={(value) => {
                  if (isThemeChoice(value)) {
                    page.actions.setTheme(value);
                  }
                }}
                value={prefs.theme}
              >
                {THEME_CHOICES.map((choice) => (
                  <MenuRadioItem data-theme-choice={choice} key={choice} value={choice}>
                    {t(`theme.${choice}`)}
                  </MenuRadioItem>
                ))}
              </MenuRadioGroup>
            </MenuGroup>
          </MenuPopup>
        </Menu>
      </div>
    </header>
  );
}

/** The banner shown while an agent holds the clock, naming the virtual time it paused at. */
export function AgentBanner() {
  const page = usePage();
  const t = useT();
  const header = useStore(page.header);
  if (header.lease !== "agent") {
    return null;
  }
  return (
    <div className="agent-banner border-b bg-muted px-4 py-2 text-center font-medium text-foreground text-sm sm:px-6" role="status">
      {t("banner.agent", { vt: header.virtualTime })}
    </div>
  );
}
