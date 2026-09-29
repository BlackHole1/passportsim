// The page's React root. The device and the firmware panel are the first two children of `<main>`
// in both modes, so a mode switch never re-mounts them (the glass canvas is transferred to the
// Worker once). The workbench stays mounted and hidden in simple mode, so card state survives.

import { useEffect, useLayoutEffect } from "react";
import { flushSync } from "react-dom";
import { createRoot } from "react-dom/client";
import type { PageModel } from "../page";
import { resolveTheme } from "../prefs";
import { Device } from "./Device";
import { Firmware } from "./Firmware";
import { AgentBanner, Header } from "./Header";
import { PageContext, usePage, useStore } from "./hooks";
import { Log } from "./Log";
import { Workbench } from "./Workbench";

export function renderApp(mount: HTMLElement, model: PageModel): void {
  const root = createRoot(mount);
  flushSync(() => {
    root.render(
      <PageContext.Provider value={model}>
        <App />
      </PageContext.Provider>,
    );
  });
}

function prefersDark(): boolean {
  try {
    return globalThis.matchMedia?.("(prefers-color-scheme: dark)").matches === true;
  } catch {
    return false;
  }
}

function App() {
  const page = usePage();
  const prefs = useStore(page.prefs);

  useLayoutEffect(() => {
    document.documentElement.lang = prefs.locale;
  }, [prefs.locale]);

  // `index.html` sets the class before the first paint; this keeps it following the choice.
  useEffect(() => {
    const apply = () => {
      document.documentElement.classList.toggle("dark", resolveTheme(prefs.theme, prefersDark()) === "dark");
    };
    apply();
    const query = globalThis.matchMedia?.("(prefers-color-scheme: dark)");
    query?.addEventListener?.("change", apply);
    return () => {
      query?.removeEventListener?.("change", apply);
    };
  }, [prefs.theme]);

  const simple = prefs.mode === "simple";
  return (
    <div className="app flex min-h-dvh flex-col" data-mode={prefs.mode}>
      <Header />
      <AgentBanner />
      <main className={simple ? "layout layout-simple" : "layout layout-advanced"}>
        <Device />
        <Firmware />
        {simple ? <Log /> : null}
        <Workbench hidden={simple} />
      </main>
    </div>
  );
}
