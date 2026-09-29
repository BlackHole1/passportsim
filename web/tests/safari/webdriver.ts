// A W3C WebDriver client for `safaridriver` over `fetch`, just large enough to hand
// `pacedSession.ts` a `SessionPage`:
//
// | `SessionPage` | WebDriver |
// |---|---|
// | `evaluate(fn, arg)` | `POST /execute/async` with `fn`'s source, which the page compiles and awaits |
// | `waitForTimeout(ms)` | a sleep in this process |
// | `locator(css).textContent()`, `.getAttribute()` | `POST /execute/sync`, polling until the element is attached, as Playwright waits |
// | `locator(css).isVisible()`, `.isEnabled()` | `POST /execute/sync`, answering at once, as Playwright does |
// | `locator(css).click()` | `POST /element` then `POST /element/{id}/click`, after polling for visible and enabled |
//
// A WebDriver element click is a trusted gesture in Safari (`isTrusted` true), which is what
// unlocks an AudioContext without fake-media flags. `evaluate` sends `fn.toString()`, so `fn` must
// not close over anything, as with Playwright.

import type { SessionLocator, SessionPage } from "../pacedSession";

const ELEMENT_KEY = "element-6066-11e4-a52e-4f735466cecf";

/** How long a locator waits for its element, as Playwright's actions wait for theirs. */
const LOCATOR_TIMEOUT_MS = 30_000;

export class WebDriverError extends Error {
  constructor(
    readonly code: string,
    message: string,
  ) {
    super(`${code}: ${message}`);
  }
}

export class WebDriverSession {
  private constructor(
    private readonly base: string,
    readonly id: string,
    readonly capabilities: Record<string, unknown>,
  ) {}

  static async open(driverUrl: string): Promise<WebDriverSession> {
    const value = (await request(driverUrl, "POST", "/session", {
      capabilities: { alwaysMatch: { browserName: "safari" } },
    })) as { sessionId: string; capabilities: Record<string, unknown> };
    return new WebDriverSession(driverUrl, value.sessionId, value.capabilities);
  }

  command(method: "GET" | "POST" | "DELETE", path: string, body?: unknown): Promise<unknown> {
    return request(this.base, method, `/session/${this.id}${path}`, body);
  }

  async close(): Promise<void> {
    await request(this.base, "DELETE", `/session/${this.id}`);
  }
}

async function request(base: string, method: string, path: string, body?: unknown): Promise<unknown> {
  const response = await fetch(`${base}${path}`, {
    method,
    headers: body === undefined ? {} : { "content-type": "application/json" },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await response.text();
  let parsed: { value?: unknown };
  try {
    parsed = JSON.parse(text) as { value?: unknown };
  } catch {
    throw new WebDriverError("invalid response", `${method} ${path} answered ${response.status}: ${text.slice(0, 200)}`);
  }
  const value = parsed.value as { error?: string; message?: string } | null | undefined;
  if (!response.ok || (value && typeof value === "object" && typeof value.error === "string")) {
    throw new WebDriverError(value?.error ?? `http ${response.status}`, value?.message ?? text.slice(0, 200));
  }
  return parsed.value;
}

/**
 * The async script `evaluate` sends: it compiles the source in the page's global scope, awaits it,
 * and answers `{ ok, value }` or `{ ok: false, error }`, so a page exception keeps its own message.
 */
const EVALUATE = `
const done = arguments[arguments.length - 1];
let fn;
try {
  fn = (0, eval)("(" + arguments[0] + ")");
} catch (error) {
  done({ ok: false, error: "the function did not compile in the page: " + String(error) });
  return;
}
const arg = arguments[1];
Promise.resolve()
  .then(() => fn(arg))
  .then(
    (value) => done({ ok: true, value: value === undefined ? null : value }),
    (error) => done({ ok: false, error: error && error.stack ? String(error.message) + "\\n" + String(error.stack) : String(error) }),
  );
`;

/** What `locator` reads about its element in one round trip; `null` when it is not attached. */
const PROBE = `
const el = document.querySelector(arguments[0]);
if (!el) return null;
const style = getComputedStyle(el);
return {
  text: el.textContent,
  attribute: arguments[1] === null ? null : el.getAttribute(arguments[1]),
  visible: el.getClientRects().length > 0 && style.visibility !== "hidden",
  enabled: !el.disabled && !el.closest("fieldset:disabled"),
};
`;

interface Probe {
  text: string | null;
  attribute: string | null;
  visible: boolean;
  enabled: boolean;
}

export class WebDriverPage implements SessionPage {
  constructor(readonly session: WebDriverSession) {}

  async prepare(width: number, height: number): Promise<void> {
    await this.session.command("POST", "/timeouts", { script: 120_000, pageLoad: 60_000, implicit: 0 });
    await this.session.command("POST", "/window/rect", { width, height, x: 0, y: 0 });
  }

  async goto(url: string): Promise<void> {
    await this.session.command("POST", "/url", { url });
  }

  async evaluate<R, A>(fn: (arg: A) => R | Promise<R>, arg: A): Promise<R> {
    const answer = (await this.session.command("POST", "/execute/async", {
      script: EVALUATE,
      args: [fn.toString(), arg === undefined ? null : arg],
    })) as { ok: true; value: R } | { ok: false; error: string };
    if (!answer.ok) {
      throw new Error(`evaluate in Safari: ${answer.error}`);
    }
    return answer.value;
  }

  async waitForTimeout(ms: number): Promise<void> {
    await Bun.sleep(ms);
  }

  async probe(selector: string, attribute: string | null = null): Promise<Probe | null> {
    return (await this.session.command("POST", "/execute/sync", {
      script: PROBE,
      args: [selector, attribute],
    })) as Probe | null;
  }

  private async attached(selector: string, attribute: string | null, actionable: boolean): Promise<Probe> {
    const deadline = Date.now() + LOCATOR_TIMEOUT_MS;
    for (;;) {
      const found = await this.probe(selector, attribute);
      if (found !== null && (!actionable || (found.visible && found.enabled))) {
        return found;
      }
      if (Date.now() > deadline) {
        throw new Error(
          `\`${selector}\` was ${found === null ? "not attached" : "not visible and enabled"} after ${LOCATOR_TIMEOUT_MS} ms`,
        );
      }
      await Bun.sleep(100);
    }
  }

  locator(selector: string): SessionLocator {
    return {
      textContent: async () => (await this.attached(selector, null, false)).text,
      getAttribute: async (name: string) => (await this.attached(selector, name, false)).attribute,
      isVisible: async () => (await this.probe(selector))?.visible ?? false,
      isEnabled: async () => (await this.probe(selector))?.enabled ?? false,
      click: async () => {
        await this.attached(selector, null, true);
        const element = (await this.session.command("POST", "/element", {
          using: "css selector",
          value: selector,
        })) as Record<string, string>;
        await this.session.command("POST", `/element/${element[ELEMENT_KEY]}/click`, {});
      },
    };
  }
}
