// The lines below are the corpus `official` menu as `ui --include-style` prints it
// (`t1_m5_official_menu_tree_prunes_to_17_lines`), trimmed to the rows each test needs.

import { describe, expect, test } from "bun:test";
import { flushSync } from "react-dom";
import { createRoot } from "react-dom/client";
import { CommandClient } from "../../api/client";
import { UiTreeController } from "../controllers";
import type { PageModel, Prefs } from "../page";
import { Store } from "../store";
import { PageContext } from "../view/hooks";
import { UiTreePane } from "../view/panes/UiTree";
import { installDom } from "../view/testDom";
import {
  EMPTY,
  MIN_READ_INTERVAL_MS,
  UiTreeFollower,
  accept,
  applyDiff,
  highlightBox,
  parseLine,
  parseTree,
  summaryLine,
  unquote,
  type GuestRect,
  type ReadOutcome,
  type UiAnswer,
} from "./uiTree";

const window = installDom();

const MENU = [
  "- obj [0,0 240x320] bg=#1689e8 e1",
  "  - obj [5,8 151x33] bg=#f4f4ea border=#17202a e25",
  '    - label "FoloToy" [42,13 76x22] e26',
  "  - obj [11,52 102x40] bg=#ffd928 border=#ffffff e28",
  '    - label "Display" [36,64 53x16] e29',
  "  - obj [123,52 102x40] bg=#f4f4ea border=#17202a e31",
  '    - label "Button" [148,64 52x16] e32',
].join("\n");

describe("the grammar read back", () => {
  test("a label line gives its depth, class, text, box and ref", () => {
    expect(parseLine('    - label "Display" [36,64 53x16] e29')).toEqual({
      depth: 2,
      cls: "label",
      text: "Display",
      rect: { x: 36, y: 64, w: 53, h: 16 },
      extras: "",
      ref: "e29",
      raw: '- label "Display" [36,64 53x16] e29',
    });
  });

  test("states, colours and value sit between the box and the ref", () => {
    const row = parseLine("  - obj [11,52 102x40] focused bg=#ffd928 border=#ffffff e28");
    expect(row.cls).toBe("obj");
    expect(row.text).toBeNull();
    expect(row.extras).toBe("focused bg=#ffd928 border=#ffffff");
    expect(row.ref).toBe("e28");
    expect(parseLine("- bar [0,0 100x8] value=30/0..100 e4").extras).toBe("value=30/0..100");
  });

  test("a negative or off-screen box is read as printed", () => {
    expect(parseLine("- obj [-20,300 50x40] e9").rect).toEqual({ x: -20, y: 300, w: 50, h: 40 });
  });

  test("a label whose text holds quotes, brackets and escapes still parses", () => {
    const row = parseLine('  - label "Audio  [FAIL] \\"x\\"\\n\\u{4e2d}" [17,111 91x16] e35');
    expect(row.text).toBe('Audio  [FAIL] "x"\n中');
    expect(row.rect).toEqual({ x: 17, y: 111, w: 91, h: 16 });
    expect(row.ref).toBe("e35");
  });

  test("a warning line is kept as a row with no class and no box", () => {
    const row = parseLine("warning: lv_obj at 0x3fca6000: the child is not walked");
    expect(row.cls).toBeNull();
    expect(row.rect).toBeNull();
    expect(row.raw).toContain("the child is not walked");
  });

  test("unquote leaves an escape it does not know as printed", () => {
    expect(unquote('"a\\qb"')).toBe("a\\qb");
    expect(unquote('"tab\\tend\\\\"')).toBe("tab\tend\\");
  });

  test("the menu parses to one row per line with its depth", () => {
    const rows = parseTree(MENU);
    expect(rows.map((row) => row.depth)).toEqual([0, 1, 2, 1, 2, 1, 2]);
    expect(rows.filter((row) => row.cls === "label").map((row) => row.text)).toEqual([
      "FoloToy",
      "Display",
      "Button",
    ]);
    expect(parseTree("")).toEqual([]);
  });
});

describe("ui --diff applied to the lines of the revision it names", () => {
  const lines = MENU.split("\n");

  test("a moved selection changes two lines in place", () => {
    const next = applyDiff(
      lines,
      [
        { line: 3, text: "  - obj [11,52 102x40] bg=#f4f4ea border=#17202a e28" },
        { line: 5, text: "  - obj [123,52 102x40] bg=#ffd928 border=#ffffff e31" },
      ],
      lines.length,
    );
    expect(next?.[3]).toContain("bg=#f4f4ea");
    expect(next?.[5]).toContain("bg=#ffd928");
    expect(next?.slice(0, 3)).toEqual(lines.slice(0, 3));
  });

  test("a shorter tree is cut to counts.shown, a longer one takes its new lines from the diff", () => {
    expect(applyDiff(lines, [], 3)).toEqual(lines.slice(0, 3));
    expect(applyDiff(lines, [{ line: 7, text: '  - label "new" [0,0 1x1] e40' }], 8)?.[7]).toBe(
      '  - label "new" [0,0 1x1] e40',
    );
  });

  test("a diff that does not fit is refused, not guessed at", () => {
    expect(applyDiff(lines, [{ line: 9, text: "x" }], 8)).toBeNull();
    expect(applyDiff(lines, [], 9)).toBeNull();
    expect(applyDiff(lines, [{ line: -1, text: "x" }], 7)).toBeNull();
  });

  test("accept applies a diff to the revision it asked about, and takes the text otherwise", () => {
    const first = accept(EMPTY, { ui_rev: 1, text: MENU, counts: { objects: 63, shown: 7 }, settled: true }, null);
    expect(first.rev).toBe(1);
    expect(first.via).toBe("full");
    expect(first.objects).toBe(63);
    const moved = "  - obj [11,52 102x40] bg=#f4f4ea border=#17202a e28";
    const second = accept(
      first,
      { ui_rev: 2, text: "ignored when the diff applies", diff: [{ line: 3, text: moved }], counts: { shown: 7 } },
      1,
    );
    expect(second.via).toBe("diff");
    expect(second.rev).toBe(2);
    expect(second.lines[3]).toBe(moved);
    expect(second.rows).toHaveLength(7);
    // An answer to a diff of some other revision is taken whole.
    const third = accept(second, { ui_rev: 3, text: "- obj [0,0 240x320] e1", diff: [], counts: { shown: 1 } }, 1);
    expect(third.via).toBe("full");
    expect(third.lines).toEqual(["- obj [0,0 240x320] e1"]);
    expect(summaryLine(third)).toBe("ui_rev 3 | 1 line(s)");
    expect(summaryLine(first)).toBe("ui_rev 1 | 7 line(s) | 63 LVGL object(s)");
    expect(summaryLine(accept(EMPTY, { ui_rev: 4, text: "", settled: false }, null))).toContain("not at a safe point");
  });
});

describe("the hover box on the glass", () => {
  const screen = { w: 240, h: 320 };

  test("the Display card at the page's integer factor", () => {
    expect(highlightBox({ x: 11, y: 52, w: 102, h: 40 }, screen, { width: 480, height: 640 })).toEqual({
      left: 22,
      top: 104,
      width: 204,
      height: 80,
    });
    expect(highlightBox({ x: 11, y: 52, w: 102, h: 40 }, screen, { width: 240, height: 320 })).toEqual({
      left: 11,
      top: 52,
      width: 102,
      height: 40,
    });
  });

  test("a factor that is not whole scales each axis on its own", () => {
    expect(highlightBox({ x: 0, y: 0, w: 240, h: 320 }, screen, { width: 360, height: 400 })).toEqual({
      left: 0,
      top: 0,
      width: 360,
      height: 400,
    });
  });

  test("the part off the screen is clipped, and a box with none on it gives nothing", () => {
    expect(highlightBox({ x: -10, y: 300, w: 30, h: 40 }, screen, { width: 480, height: 640 })).toEqual({
      left: 0,
      top: 600,
      width: 40,
      height: 40,
    });
    expect(highlightBox({ x: 240, y: 0, w: 10, h: 10 }, screen, { width: 480, height: 640 })).toBeNull();
    expect(highlightBox({ x: 0, y: 0, w: 0, h: 10 }, screen, { width: 480, height: 640 })).toBeNull();
    expect(highlightBox({ x: 0, y: 0, w: 10, h: 10 }, screen, { width: 0, height: 0 })).toBeNull();
  });
});

function rig(answers: Array<(args: { diff?: number }) => ReadOutcome>) {
  const reads: Array<{ diff?: number; include_style: true }> = [];
  const timers: Array<{ fn: () => void; ms: number }> = [];
  const errors: Array<string | null> = [];
  let clock = 0;
  let rev = 0;
  const follower = new UiTreeFollower({
    read: (args) => {
      reads.push(args);
      const next = answers.shift();
      if (next) {
        return Promise.resolve(next(args));
      }
      rev += 1;
      return Promise.resolve({ ok: true, json: { ui_rev: rev, text: MENU, counts: { shown: 7 } } });
    },
    now: () => clock,
    schedule: (fn, ms) => timers.push({ fn, ms }),
    onState: () => {},
    onError: (message) => errors.push(message),
  });
  return {
    follower,
    reads,
    timers,
    errors,
    advance(ms: number) {
      clock += ms;
    },
    runTimers() {
      const due = timers.splice(0);
      for (const timer of due) {
        timer.fn();
      }
    },
  };
}

const settle = () => new Promise((resolve) => setTimeout(resolve, 0));

describe("when the tab reads", () => {
  test("a hidden tab reads nothing and schedules nothing, whatever the screen does", async () => {
    const r = rig([]);
    r.follower.changed();
    r.follower.changed();
    await settle();
    expect(r.reads).toEqual([]);
    expect(r.timers).toEqual([]);
  });

  test("opening reads the whole tree, and a change reads the diff after the interval", async () => {
    const r = rig([]);
    r.follower.setVisible(true);
    await settle();
    expect(r.reads).toEqual([{ include_style: true }]);
    expect(r.follower.state.rev).toBe(1);
    // A frame 10 ms later waits out the interval on one timer, however many frames come.
    r.advance(10);
    r.follower.changed();
    r.follower.changed();
    expect(r.reads).toHaveLength(1);
    expect(r.timers.map((timer) => timer.ms)).toEqual([MIN_READ_INTERVAL_MS - 10]);
    r.advance(MIN_READ_INTERVAL_MS);
    r.runTimers();
    await settle();
    expect(r.reads[1]).toEqual({ diff: 1, include_style: true });
    expect(r.follower.state.rev).toBe(2);
  });

  test("closing the tab before a scheduled read cancels it", async () => {
    const r = rig([]);
    r.follower.setVisible(true);
    await settle();
    r.follower.changed();
    r.follower.setVisible(false);
    r.runTimers();
    await settle();
    expect(r.reads).toHaveLength(1);
  });

  test("a diff refused because another reader took a revision is followed by a whole read", async () => {
    const r = rig([
      () => ({ ok: true, json: { ui_rev: 1, text: MENU, counts: { shown: 7 } } }),
      () => ({
        ok: false,
        code: "E_USAGE",
        message: "`diff` names revision 1, and the newest kept revision is 2",
      }),
      () => ({ ok: true, json: { ui_rev: 3, text: MENU, counts: { shown: 7 } } }),
    ]);
    r.follower.setVisible(true);
    await settle();
    r.advance(MIN_READ_INTERVAL_MS);
    r.follower.changed();
    await settle();
    await settle();
    expect(r.reads).toEqual([{ include_style: true }, { diff: 1, include_style: true }, { include_style: true }]);
    expect(r.follower.state.rev).toBe(3);
    expect(r.errors.filter((e) => e !== null)).toEqual([]);
  });

  test("any other refusal is shown, and the next good read clears it", async () => {
    const r = rig([
      () => ({ ok: false, code: "E_STATE", message: "no DWARF definition of struct _lv_global_t" }),
    ]);
    r.follower.setVisible(true);
    await settle();
    expect(r.errors).toEqual(["E_STATE: no DWARF definition of struct _lv_global_t"]);
    r.advance(MIN_READ_INTERVAL_MS);
    r.follower.changed();
    await settle();
    expect(r.errors.at(-1)).toBeNull();
    expect(r.reads[1]).toEqual({ include_style: true });
  });
});

describe("the pane", () => {
  function render(controller: UiTreeController): HTMLElement {
    const root = window.document.createElement("div") as unknown as HTMLElement;
    const model = {
      uiTree: controller,
      prefs: new Store<Prefs>({ mode: "advanced", locale: "en", theme: "light", zoom: "140", follow: true }),
    } as unknown as PageModel;
    flushSync(() => {
      createRoot(root).render(
        <PageContext.Provider value={model}>
          <UiTreePane />
        </PageContext.Provider>,
      );
    });
    return root;
  }

  function mount(answer: (args: Record<string, unknown>) => UiAnswer) {
    const sent: Array<Record<string, unknown>> = [];
    const highlights: Array<GuestRect | null> = [];
    const client = new CommandClient((request) => {
      const call = JSON.parse(request) as { cmd: string; args: Record<string, unknown> };
      sent.push(call.args);
      return Promise.resolve({ ok: JSON.stringify({ json: answer(call.args), text: "" }) });
    });
    const controller = new UiTreeController({
      client,
      confirm: () => true,
      highlight: (rect) => highlights.push(rect),
      now: () => 0,
      schedule: () => {},
    });
    return { controller, root: render(controller), sent, highlights };
  }

  const over = (node: Element, type: "mouseover" | "mouseout") => {
    node.dispatchEvent(new window.MouseEvent(type, { bubbles: true }) as unknown as Event);
  };

  test("renders the rows indented, and hovering one outlines its box", async () => {
    const { controller, root, sent, highlights } = mount(() => ({
      ui_rev: 5,
      text: MENU,
      counts: { objects: 63, shown: 7 },
      screen: { w: 240, h: 320 },
      settled: true,
    }));
    controller.setVisible(true);
    await settle();
    expect(sent).toEqual([{ include_style: true }]);
    const rows = [...root.querySelectorAll(".ui-row")] as HTMLElement[];
    expect(rows).toHaveLength(7);
    const display = root.querySelector('[data-ref="e29"]') as HTMLElement;
    expect(display.textContent).toBe('- label "Display" [36,64 53x16] e29');
    expect(display.dataset.class).toBe("label");
    expect(display.getAttribute("aria-level")).toBe("3");
    expect(display.style.paddingLeft).toBe("2.5em");
    expect(root.querySelector("[data-ui-summary]")?.textContent).toBe("ui_rev 5 | 7 line(s) | 63 LVGL object(s)");
    over(display, "mouseover");
    expect(highlights.at(-1)).toEqual({ x: 36, y: 64, w: 53, h: 16 });
    over(display, "mouseout");
    expect(highlights.at(-1)).toBeNull();
    // Hiding the tab clears a box that is still up.
    over(display, "mouseover");
    controller.setVisible(false);
    expect(highlights.at(-1)).toBeNull();
  });

  test("a refusal is written on the pane", async () => {
    const client = new CommandClient(() =>
      Promise.resolve({ err: JSON.stringify({ code: "E_STATE", message: "LVGL has no default display yet" }) }),
    );
    const controller = new UiTreeController({ client, confirm: () => true, highlight: () => {}, now: () => 0, schedule: () => {} });
    const root = render(controller);
    controller.setVisible(true);
    await settle();
    const error = root.querySelector(".card-error") as HTMLElement;
    expect(error.hidden).toBe(false);
    expect(error.textContent).toBe("E_STATE: LVGL has no default display yet");
  });
});
