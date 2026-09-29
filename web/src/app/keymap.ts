// The keyboard map: every device control has a key, so a user without a pointer reaches UP, OK,
// DOWN and POWER. Whether a key belongs to the device or a text field is decided here, so the
// console input's Enter never presses OK.

import type { ButtonName } from "../api/commands";

export type KeyAction =
  | { readonly kind: "button"; readonly button: ButtonName }
  | { readonly kind: "power" }
  | { readonly kind: "tab"; readonly delta: number }
  | null;

/** `ArrowLeft` and `ArrowRight` move between tabs: the strip's ARIA behaviour, not a device control. */
const BINDINGS: Readonly<Record<string, KeyAction>> = {
  ArrowUp: { kind: "button", button: "up" },
  ArrowDown: { kind: "button", button: "down" },
  Enter: { kind: "button", button: "ok" },
  " ": { kind: "button", button: "ok" },
  p: { kind: "power" },
  P: { kind: "power" },
  ArrowLeft: { kind: "tab", delta: -1 },
  ArrowRight: { kind: "tab", delta: 1 },
};

export interface KeyInput {
  readonly key: string;
  readonly repeat?: boolean;
  readonly ctrlKey?: boolean;
  readonly metaKey?: boolean;
  readonly altKey?: boolean;
  readonly inTextField?: boolean;
}

/**
 * The action a key press means, or `null`. A modifier combination is never a device control:
 * `Ctrl+P` is the browser's print dialog.
 */
export function keyAction(event: KeyInput): KeyAction {
  if (event.ctrlKey === true || event.metaKey === true || event.altKey === true) {
    return null;
  }
  const action = BINDINGS[event.key] ?? null;
  if (action === null) {
    return null;
  }
  if (event.inTextField === true) {
    return null;
  }
  return action;
}

export function isTextField(target: unknown): boolean {
  if (typeof target !== "object" || target === null) {
    return false;
  }
  const node = target as { tagName?: unknown; isContentEditable?: unknown };
  const tag = typeof node.tagName === "string" ? node.tagName.toUpperCase() : "";
  return (
    tag === "INPUT" ||
    tag === "TEXTAREA" ||
    tag === "SELECT" ||
    node.isContentEditable === true
  );
}

const ACTIVATES =
  'button, a[href], summary, [role="button"], [role="tab"], [role="menuitem"], [role="menuitemradio"], ' +
  '[role="menuitemcheckbox"], [role="option"], [role="checkbox"], [role="switch"], [role="radio"]';

const NAVIGATES =
  '[role="tablist"], [role="menu"], [role="listbox"], [role="radiogroup"], [role="slider"], [role="tree"], ' +
  '[data-slot="toggle-group"]';

/**
 * Whether the focused widget owns the key: Enter on a button, arrows in a tab list or menu. The
 * device's own buttons (`data-control`) are not such widgets; focus on one still drives the device.
 */
export function widgetOwnsKey(key: string, target: unknown): boolean {
  const node = target as { closest?: (selector: string) => unknown } | null;
  if (typeof node !== "object" || node === null || typeof node.closest !== "function") {
    return false;
  }
  if (node.closest("[data-control]")) {
    return false;
  }
  if (key === "Enter" || key === " ") {
    return Boolean(node.closest(ACTIVATES));
  }
  if (key.startsWith("Arrow")) {
    return Boolean(node.closest(NAVIGATES));
  }
  return false;
}

export const DEVICE_BINDINGS: readonly {
  readonly control: ButtonName | "power";
  readonly key: string;
}[] = [
  { control: "up", key: "ArrowUp" },
  { control: "ok", key: "Enter" },
  { control: "down", key: "ArrowDown" },
  { control: "power", key: "p" },
];
