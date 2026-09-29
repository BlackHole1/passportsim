import { describe, expect, test } from "bun:test";
import { RingId, SerialStream } from "../worker/layout";
import {
  ConsoleModel,
  MAX_LINE_CHARS,
  channelOf,
  compileFilter,
  lineStamp,
  presentLine,
  streamOf,
  stripAnsi,
  type SerialSliceIn,
} from "./console";

const ESC = "";
const encoder = new TextEncoder();

function slice(stream: number, text: string, startCursor = 0n, dropped = 0n): SerialSliceIn {
  const bytes = encoder.encode(text);
  const lines: { offset: bigint; vtPs: bigint }[] = [];
  let at = startCursor;
  for (const byte of bytes) {
    if (byte === 0x0a) {
      lines.push({ offset: at, vtPs: (at + 1n) * 1_000_000_000n });
    }
    at += 1n;
  }
  return { stream, bytes, dropped, lines, linesDropped: 0n };
}

describe("stream naming", () => {
  test("the byte rings map to the channel names the `serial` command uses", () => {
    expect(channelOf(RingId.UsjTx)).toBe("usj");
    expect(channelOf(RingId.Uart0Tx)).toBe("uart0");
    expect(streamOf("usj")).toBe(SerialStream.UsjTx);
    expect(streamOf("uart0")).toBe(SerialStream.Uart0Tx);
  });
});

describe("line presentation", () => {
  test("ANSI is stripped", () => {
    expect(stripAnsi(`${ESC}[0;32mI (438) bsp_batt: ok${ESC}[0m`)).toBe("I (438) bsp_batt: ok");
    expect(stripAnsi(`${ESC}[2K${ESC}[1Gprogress`)).toBe("progress");
    expect(stripAnsi("no escapes here")).toBe("no escapes here");
  });

  test("lines are capped at 400 characters", () => {
    const long = "x".repeat(500);
    const shown = presentLine(long);
    expect(shown.length).toBe(MAX_LINE_CHARS + 3);
    expect(shown.endsWith("...")).toBe(true);
    expect(presentLine("x".repeat(MAX_LINE_CHARS)).length).toBe(MAX_LINE_CHARS);
  });

  test("a CRLF stream does not leave a carriage return on every line", () => {
    expect(presentLine("ready\r")).toBe("ready");
  });
});

describe("ConsoleModel", () => {
  test("it renders a scripted boot stream as lines with their cursors and times", () => {
    const model = new ConsoleModel();
    model.push(slice(RingId.UsjTx, "ESP-ROM:esp32c3-api1-20210207\nI (31) boot: ESP-IDF\n"));
    const lines = model.all();
    expect(lines.map((line) => line.text)).toEqual([
      "ESP-ROM:esp32c3-api1-20210207",
      "I (31) boot: ESP-IDF",
    ]);
    expect(lines[0]?.cursor).toBe(29n);
    expect(lines[0]?.vtPs).toBe(30n * 1_000_000_000n);
    expect(lines[0]?.stream).toBe("usj");
  });

  test("a line split across two slices is one line, not two", () => {
    const model = new ConsoleModel();
    model.push(slice(RingId.UsjTx, "pk_app: re"));
    expect(model.all()).toHaveLength(0);
    model.push(slice(RingId.UsjTx, "ady\n", 10n));
    expect(model.all().map((line) => line.text)).toEqual(["pk_app: ready"]);
    expect(model.all()[0]?.cursor).toBe(13n);
  });

  test("both streams land in one pane in the order they arrived", () => {
    const model = new ConsoleModel();
    model.push(slice(RingId.Uart0Tx, "bootloader\n"));
    model.push(slice(RingId.UsjTx, "app\n"));
    expect(model.all().map((line) => [line.stream, line.text])).toEqual([
      ["uart0", "bootloader"],
      ["usj", "app"],
    ]);
  });

  test("evicted bytes become a visible gap and the partial line is abandoned", () => {
    const model = new ConsoleModel();
    model.push(slice(RingId.UsjTx, "half of a li"));
    model.push(slice(RingId.UsjTx, "after the hole\n", 4_096n, 4_096n));
    const lines = model.all();
    expect(lines[0]?.gap).toBe(true);
    expect(lines[0]?.text).toContain("4096 bytes lost");
    expect(lines[1]?.text).toBe("after the hole");
    // The abandoned partial never appears: nothing knows what the rest of it said.
    expect(lines.some((line) => line.text.includes("half of a li"))).toBe(false);
  });

  test("identical consecutive lines collapse", () => {
    const model = new ConsoleModel();
    model.push(slice(RingId.UsjTx, "W (100) wdt: late\n".repeat(12)));
    expect(model.all()).toHaveLength(1);
    expect(model.all()[0]?.repeat).toBe(12);
  });

  test("the same text on the other stream does not collapse into it", () => {
    const model = new ConsoleModel();
    model.push(slice(RingId.UsjTx, "same\n"));
    model.push(slice(RingId.Uart0Tx, "same\n"));
    expect(model.all()).toHaveLength(2);
  });

  test("a repeat count never spans a gap", () => {
    const model = new ConsoleModel();
    model.push(slice(RingId.UsjTx, "same\n"));
    model.push(slice(RingId.UsjTx, "same\n", 100n, 100n));
    expect(model.all().map((line) => [line.gap, line.repeat])).toEqual([
      [false, 1],
      [true, 1],
      [false, 1],
    ]);
  });

  test("the scrollback is bounded and says how many rows it lost", () => {
    const model = new ConsoleModel(4);
    for (let index = 0; index < 10; index += 1) {
      model.push(slice(RingId.UsjTx, `line ${index}\n`));
    }
    expect(model.all()).toHaveLength(4);
    expect(model.scrolledOut).toBe(6);
    expect(model.all()[0]?.text).toBe("line 6");
  });

  test("the primary lines are the USB console's once it has printed, else every stream's", () => {
    const model = new ConsoleModel();
    model.push(slice(RingId.Uart0Tx, "ESP-ROM\n"));
    expect(model.primary().map((line) => line.stream)).toEqual(["uart0"]);
    expect(model.streamsSeen()).toEqual(["uart0"]);
    model.push(slice(RingId.UsjTx, "ESP-ROM\n"));
    model.push(slice(RingId.Uart0Tx, "lost\n", 100n, 50n));
    expect(model.primary().map((line) => [line.stream, line.text])).toEqual([["usj", "ESP-ROM"]]);
    expect(model.streamsSeen()).toEqual(["uart0", "usj"]);
    model.clear();
    expect(model.streamsSeen()).toEqual([]);
  });

  test("clearing resets the byte cursors, which a reboot restarts at zero", () => {
    const model = new ConsoleModel();
    model.push(slice(RingId.UsjTx, "first\n"));
    model.clear();
    model.push(slice(RingId.UsjTx, "second\n"));
    expect(model.all().map((line) => line.text)).toEqual(["second"]);
    expect(model.all()[0]?.cursor).toBe(6n);
  });
});

describe("the line filter", () => {
  test("a substring filter is case-insensitive", () => {
    const model = new ConsoleModel();
    model.push(slice(RingId.UsjTx, "I (1) bsp_batt: ok\nI (2) pk_app: ready\n"));
    model.setFilter("PK_APP");
    expect(model.visible().map((line) => line.text)).toEqual(["I (2) pk_app: ready"]);
  });

  test("a /regex/ filter is compiled as one", () => {
    const model = new ConsoleModel();
    model.push(slice(RingId.UsjTx, "I (1) a\nE (2) b\n"));
    model.setFilter("/^E /");
    expect(model.visible().map((line) => line.text)).toEqual(["E (2) b"]);
  });

  test("a closed but invalid pattern searches for its literal part rather than throwing", () => {
    const filter = compileFilter("/pk_a[/");
    expect(filter?.source).toBe("/pk_a[/");
    expect(filter?.matches({ text: "x pk_a[ y" } as never)).toBe(true);
    expect(filter?.matches({ text: "nothing" } as never)).toBe(false);
  });

  test("a pattern the user has not closed yet is a literal substring search", () => {
    const filter = compileFilter("/pk_a");
    expect(filter?.matches({ text: "path /pk_a here" } as never)).toBe(true);
    expect(filter?.matches({ text: "pk_a without the slash" } as never)).toBe(false);
  });

  test("a global flag is dropped, so lastIndex cannot make every other line miss", () => {
    const filter = compileFilter("/ready/g");
    expect(filter?.matches({ text: "pk_app: ready" } as never)).toBe(true);
    expect(filter?.matches({ text: "pk_app: ready" } as never)).toBe(true);
  });

  test("an empty filter shows everything", () => {
    const model = new ConsoleModel();
    model.push(slice(RingId.UsjTx, "a\nb\n"));
    model.setFilter("   ");
    expect(model.visible()).toHaveLength(2);
  });

  test("a gap row survives every filter, so a filtered view never claims continuity", () => {
    const model = new ConsoleModel();
    model.push(slice(RingId.UsjTx, "keep me\n"));
    model.push(slice(RingId.UsjTx, "drop me\n", 64n, 64n));
    model.setFilter("keep");
    expect(model.visible().map((line) => line.gap)).toEqual([false, true]);
  });
});

describe("capture mode", () => {
  test("it collects what was printed while it was on and restarts each time", () => {
    const model = new ConsoleModel();
    model.push(slice(RingId.UsjTx, "before\n"));
    model.setCapture(true);
    model.push(slice(RingId.UsjTx, "during\n"));
    expect(model.captureText()).toBe("during");
    model.setCapture(false);
    model.push(slice(RingId.UsjTx, "after\n"));
    expect(model.captureText()).toBe("during");
  });
});

describe("the line-state indicators", () => {
  test("DTR and RTS start clear and are set from the transport", () => {
    const model = new ConsoleModel();
    expect(model.state).toEqual({ dtr: false, rts: false });
    model.setLineState({ dtr: true, rts: false });
    expect(model.state).toEqual({ dtr: true, rts: false });
  });
});

describe("lineStamp", () => {
  test("a stamped line shows its virtual time and an unstamped one its cursor", () => {
    expect(lineStamp({ vtPs: 12_402n * 1_000_000_000n, cursor: 7n } as never)).toBe("12.402");
    expect(lineStamp({ vtPs: null, cursor: 7n } as never)).toBe("@7");
  });
});
