// The panel tabs and environment cards, declared once: the views, keyboard stepping and Playwright
// ids (`#tab-<id>`, `#pane-<id>`) all derive from these lists.

export interface TabSpec {
  readonly id: "console" | "ui-tree" | "events" | "inspect" | "fidelity" | "perf";
}

export const TABS: readonly TabSpec[] = [
  { id: "console" },
  { id: "ui-tree" },
  { id: "events" },
  { id: "inspect" },
  { id: "fidelity" },
  { id: "perf" },
];

export interface CardSpec {
  readonly id: "battery" | "usb" | "audio" | "nfc" | "wifi" | "ble" | "snapshots";
}

export const CARDS: readonly CardSpec[] = [
  { id: "battery" },
  { id: "usb" },
  { id: "audio" },
  { id: "nfc" },
  { id: "wifi" },
  { id: "ble" },
  { id: "snapshots" },
];

export const DEFAULT_TAB = "console";

export class TabState {
  private current: string;
  private readonly collapsed = new Set<string>();
  private readonly openedNarrow = new Set<string>();

  constructor(
    private readonly tabs: readonly TabSpec[] = TABS,
    initial: string = DEFAULT_TAB,
  ) {
    this.current = this.has(initial) ? initial : (tabs[0]?.id ?? DEFAULT_TAB);
  }

  get active(): string {
    return this.current;
  }

  has(id: string): boolean {
    return this.tabs.some((tab) => tab.id === id);
  }

  /** Opens a tab. An unknown id (a stale URL fragment) is ignored rather than thrown on. */
  select(id: string): boolean {
    if (!this.has(id) || id === this.current) {
      return false;
    }
    this.current = id;
    return true;
  }

  step(delta: number): string {
    const at = this.tabs.findIndex((tab) => tab.id === this.current);
    const count = this.tabs.length;
    if (count === 0) {
      return this.current;
    }
    const next = this.tabs[(((at + delta) % count) + count) % count];
    this.current = next?.id ?? this.current;
    return this.current;
  }

  /**
   * Whether a card is showing only its header. The narrow layout collapses every card by default and
   * the user may collapse one at any width; the two are tracked apart so widening the window restores
   * exactly the cards the user had open.
   */
  isCollapsed(cardId: string, narrow: boolean): boolean {
    return narrow ? !this.openedNarrow.has(cardId) : this.collapsed.has(cardId);
  }

  toggleCard(cardId: string, narrow = false): void {
    const set = narrow ? this.openedNarrow : this.collapsed;
    if (set.has(cardId)) {
      set.delete(cardId);
    } else {
      set.add(cardId);
    }
  }
}
