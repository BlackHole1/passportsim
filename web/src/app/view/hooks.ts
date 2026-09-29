import { createContext, useContext, useEffect, useRef, useSyncExternalStore } from "react";
import { translator, type Locale, type Translate } from "../i18n";
import type { PageModel } from "../page";
import type { Store } from "../store";

export const PageContext = createContext<PageModel | null>(null);

export function usePage(): PageModel {
  const page = useContext(PageContext);
  if (page === null) {
    throw new Error("a view was rendered outside the page");
  }
  return page;
}

export function useStore<T>(store: Store<T>): T {
  return useSyncExternalStore(store.subscribe, store.snapshot, store.snapshot);
}

const translators = new Map<Locale, Translate>();

export function useT(): Translate {
  const locale = useStore(usePage().prefs).locale;
  let t = translators.get(locale);
  if (t === undefined) {
    t = translator(locale);
    translators.set(locale, t);
  }
  return t;
}

/**
 * Calls `handler` on the native `change` event. React's `onChange` is `input`, which fires per pixel
 * of a drag or per keystroke, and each would be a journaled command.
 */
export function useNativeChange<E extends HTMLElement>(handler: (element: E) => void) {
  const ref = useRef<E | null>(null);
  const latest = useRef(handler);
  latest.current = handler;
  useEffect(() => {
    const node = ref.current;
    if (!node) {
      return;
    }
    const listener = () => {
      latest.current(node);
    };
    node.addEventListener("change", listener);
    return () => {
      node.removeEventListener("change", listener);
    };
  }, []);
  return ref;
}
