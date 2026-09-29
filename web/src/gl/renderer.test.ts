// Rows uploaded, allocations and context loss, against a recording context. Real GPU pixels are
// checked by `web/tests/gl.spec.ts`.

import { describe, expect, test } from "bun:test";

import { FRAME_HEIGHT, FRAME_WIDTH } from "../worker/layout";
import { NO_HIGHP_REASON, hasFragmentHighp, openDisplay, type DisplayCanvas } from "./display";
import { PanelRenderer, type Gl } from "./renderer";
import type { PanelView } from "./rgb565";
import type { DisplayState } from "./sink";

const LIT: PanelView = { backlight: 1024, backlightScale: 1024, glassComplement: false, powered: true, sleeping: false, displayOn: true };

type Call = { name: string; args: unknown[] };

function recordingGl(fragmentHighpPrecision = 23): { gl: Gl; calls: Call[] } {
  const calls: Call[] = [];
  const constants = {
    TEXTURE_2D: 1, RGB: 2, UNSIGNED_SHORT_5_6_5: 3, ARRAY_BUFFER: 4, STATIC_DRAW: 5, FLOAT: 6,
    TEXTURE_MIN_FILTER: 7, TEXTURE_MAG_FILTER: 8, NEAREST: 9, TEXTURE_WRAP_S: 10, TEXTURE_WRAP_T: 11,
    CLAMP_TO_EDGE: 12, UNPACK_ALIGNMENT: 13, UNPACK_FLIP_Y_WEBGL: 14, VERTEX_SHADER: 15,
    FRAGMENT_SHADER: 16, LINK_STATUS: 17, COMPILE_STATUS: 18, COLOR_BUFFER_BIT: 19, TRIANGLES: 20,
    HIGH_FLOAT: 21,
  };
  const recorded = new Set(["texImage2D", "texSubImage2D", "viewport", "drawArrays", "uniform1f", "createTexture", "clear"]);
  const gl = new Proxy(constants, {
    get(target, key: string) {
      if (key in target) {
        return target[key as keyof typeof target];
      }
      return (...args: unknown[]) => {
        if (recorded.has(key)) {
          calls.push({ name: key, args });
        }
        if (key.startsWith("create") || key === "getUniformLocation") {
          return {};
        }
        if (key === "getProgramParameter" || key === "getShaderParameter") {
          return true;
        }
        if (key === "getShaderPrecisionFormat") {
          const precision = args[1] === constants.HIGH_FLOAT ? fragmentHighpPrecision : 8;
          return { rangeMin: precision > 0 ? 62 : 0, rangeMax: precision > 0 ? 62 : 0, precision };
        }
        return 0;
      };
    },
  }) as unknown as Gl;
  return { gl, calls };
}

function named(calls: Call[], name: string): Call[] {
  return calls.filter((call) => call.name === name);
}

describe("uploading", () => {
  test("sends exactly the rows asked for, clamped, from a view starting at the first row", () => {
    const { gl, calls } = recordingGl();
    const renderer = PanelRenderer.create(gl, FRAME_WIDTH, FRAME_HEIGHT);
    const pixels = new Uint16Array(FRAME_WIDTH * FRAME_HEIGHT);
    pixels[5 * FRAME_WIDTH] = 0xabcd;
    renderer.upload(pixels, 5, 7);
    renderer.upload(pixels, 318, 900);
    renderer.upload(pixels, 9, 3);
    const uploads = named(calls, "texSubImage2D");
    expect(uploads).toHaveLength(2);
    expect(uploads[0]?.args.slice(2, 8)).toEqual([0, 5, FRAME_WIDTH, 3, 2, 3]);
    expect((uploads[0]?.args[8] as Uint16Array)[0]).toBe(0xabcd);
    expect(uploads[1]?.args.slice(3, 5)).toEqual([318, FRAME_WIDTH]);
    expect(uploads[1]?.args[5]).toBe(2);
    expect(renderer.rowsUploaded).toBe(5);
  });

  test("reuses its row views and viewport frame after frame", () => {
    const { gl, calls } = recordingGl();
    const renderer = PanelRenderer.create(gl, FRAME_WIDTH, FRAME_HEIGHT);
    const pixels = new Uint16Array(FRAME_WIDTH * FRAME_HEIGHT);
    renderer.upload(pixels, 40, 41);
    const firstView = named(calls, "texSubImage2D")[0]?.args[8];
    const firstViewport = renderer.draw(LIT, 480, 640);
    for (let frame = 0; frame < 50; frame += 1) {
      renderer.upload(pixels, 40, 41);
      expect(renderer.draw(LIT, 480, 640)).toBe(firstViewport);
    }
    expect(named(calls, "texSubImage2D").every((call) => call.args[8] === firstView)).toBe(true);
  });

  test("skips a detached view and sends every row once the view is whole again", () => {
    const { gl, calls } = recordingGl();
    const renderer = PanelRenderer.create(gl, FRAME_WIDTH, FRAME_HEIGHT);
    renderer.upload(new Uint16Array(0), 3, 3);
    expect(named(calls, "texSubImage2D")).toHaveLength(0);
    renderer.upload(new Uint16Array(FRAME_WIDTH * FRAME_HEIGHT), 3, 3);
    expect(named(calls, "texSubImage2D")[0]?.args.slice(3, 6)).toEqual([0, FRAME_WIDTH, FRAME_HEIGHT]);
  });
});

describe("drawing", () => {
  test("clears the canvas, then draws into the integer viewport with the panel's uniforms", () => {
    const { gl, calls } = recordingGl();
    const renderer = PanelRenderer.create(gl, FRAME_WIDTH, FRAME_HEIGHT);
    renderer.draw({ ...LIT, glassComplement: true, backlight: 256 }, 500, 700);
    expect(named(calls, "viewport").map((call) => call.args)).toEqual([
      [0, 0, 500, 700],
      [10, 30, 480, 640],
    ]);
    expect(named(calls, "uniform1f").map((call) => call.args[1])).toEqual([0.25, 1]);
    expect(named(calls, "drawArrays")).toHaveLength(1);
  });

  test("sends gain 0 for a sleeping or unpowered panel", () => {
    const { gl, calls } = recordingGl();
    const renderer = PanelRenderer.create(gl, FRAME_WIDTH, FRAME_HEIGHT);
    renderer.draw({ ...LIT, sleeping: true }, 240, 320);
    renderer.draw({ ...LIT, powered: false }, 240, 320);
    expect(named(calls, "uniform1f").map((call) => call.args[1])).toEqual([0, 0, 0, 0]);
  });
});

describe("context loss", () => {
  function lossSetup() {
    const { gl, calls } = recordingGl();
    const canvas = new EventTarget();
    const states: DisplayState[] = [];
    const renderer = PanelRenderer.create(gl, FRAME_WIDTH, FRAME_HEIGHT, canvas, (state) => {
      states.push(state);
    });
    return { calls, canvas, states, renderer };
  }

  test("prevents the default so the browser restores, and reports the loss", () => {
    const { canvas, states, renderer } = lossSetup();
    const lost = new Event("webglcontextlost", { cancelable: true });
    canvas.dispatchEvent(lost);
    expect(lost.defaultPrevented).toBe(true);
    expect(renderer.isLost).toBe(true);
    expect(states).toEqual([{ backend: "webgl1", reason: null, contextLost: true }]);
  });

  test("touches no GL while lost", () => {
    const { calls, canvas, renderer } = lossSetup();
    canvas.dispatchEvent(new Event("webglcontextlost", { cancelable: true }));
    const before = calls.length;
    renderer.upload(new Uint16Array(FRAME_WIDTH * FRAME_HEIGHT), 0, 10);
    renderer.draw(LIT, 240, 320);
    expect(calls.length).toBe(before);
  });

  test("rebuilds, re-uploads the whole last frame and redraws it on restore", () => {
    const { calls, canvas, states, renderer } = lossSetup();
    const pixels = new Uint16Array(FRAME_WIDTH * FRAME_HEIGHT).fill(0x07e0);
    renderer.upload(pixels, 100, 100);
    renderer.draw({ ...LIT, glassComplement: true }, 480, 640);
    canvas.dispatchEvent(new Event("webglcontextlost", { cancelable: true }));
    // A slice painted while the context was gone; its rows must not be lost.
    renderer.upload(pixels, 7, 7);
    const mark = calls.length;
    canvas.dispatchEvent(new Event("webglcontextrestored"));

    const after = calls.slice(mark);
    expect(named(after, "createTexture")).toHaveLength(1);
    expect(named(after, "texImage2D")).toHaveLength(1);
    const uploads = named(after, "texSubImage2D");
    expect(uploads).toHaveLength(1);
    expect(uploads[0]?.args.slice(3, 6)).toEqual([0, FRAME_WIDTH, FRAME_HEIGHT]);
    expect(named(after, "drawArrays")).toHaveLength(1);
    expect(named(after, "viewport").at(-1)?.args).toEqual([0, 0, 480, 640]);
    expect(named(after, "uniform1f").map((call) => call.args[1])).toEqual([1, 1]);
    expect(renderer.isLost).toBe(false);
    expect(states.at(-1)).toEqual({ backend: "webgl1", reason: null, contextLost: false });

    renderer.upload(pixels, 9, 9);
    expect(named(calls, "texSubImage2D").at(-1)?.args.slice(3, 6)).toEqual([9, FRAME_WIDTH, 1]);
  });

  // The restored texture is zeros, which the glass complement turns white.
  test("clears to black instead of drawing the empty texture when the last frame was detached", () => {
    const { calls, canvas, renderer } = lossSetup();
    const memory = new WebAssembly.Memory({ initial: 3 });
    const pixels = new Uint16Array(memory.buffer, 0, FRAME_WIDTH * FRAME_HEIGHT);
    renderer.upload(pixels, 0, FRAME_HEIGHT - 1);
    renderer.draw({ ...LIT, glassComplement: true }, 240, 320);
    canvas.dispatchEvent(new Event("webglcontextlost", { cancelable: true }));
    memory.grow(1);
    expect(pixels.length).toBe(0);
    const mark = calls.length;
    canvas.dispatchEvent(new Event("webglcontextrestored"));

    let after = calls.slice(mark);
    expect(named(after, "texSubImage2D")).toHaveLength(0);
    expect(named(after, "clear")).toHaveLength(1);
    expect(named(after, "drawArrays")).toHaveLength(0);
    renderer.draw({ ...LIT, glassComplement: true }, 240, 320);
    expect(named(calls.slice(mark), "drawArrays")).toHaveLength(0);

    // A partial upload of a whole view is widened to every row, and drawing resumes.
    renderer.upload(new Uint16Array(FRAME_WIDTH * FRAME_HEIGHT), 5, 5);
    renderer.draw({ ...LIT, glassComplement: true }, 240, 320);
    after = calls.slice(mark);
    expect(named(after, "texSubImage2D")[0]?.args.slice(3, 6)).toEqual([0, FRAME_WIDTH, FRAME_HEIGHT]);
    expect(named(after, "drawArrays")).toHaveLength(1);
  });
});

describe("choosing a display", () => {
  function canvasWith(webgl: Gl | null, twoD: unknown): DisplayCanvas & { asked: string[] } {
    const asked: string[] = [];
    return {
      asked,
      addEventListener: () => {},
      getContext: ((kind: string) => {
        asked.push(kind);
        return kind === "webgl" ? webgl : twoD;
      }) as DisplayCanvas["getContext"],
    };
  }

  test("takes WebGL1 when the canvas has it, and never asks for 2D", () => {
    const canvas = canvasWith(recordingGl().gl, {});
    const display = openDisplay(canvas, FRAME_WIDTH, FRAME_HEIGHT);
    expect(display.sink?.backend).toBe("webgl1");
    expect(display.state).toEqual({ backend: "webgl1", reason: null, contextLost: false });
    expect(canvas.asked).toEqual(["webgl"]);
  });

  test("falls back to 2D and says so when WebGL1 is unavailable", () => {
    const scratch = { createImageData: (w: number, h: number) => ({ width: w, height: h, data: new Uint8ClampedArray(w * h * 4) }), putImageData: () => {} };
    const display = openDisplay(canvasWith(null, {}), FRAME_WIDTH, FRAME_HEIGHT, undefined, () => ({ canvas: {}, context: scratch }));
    expect(display.sink?.backend).toBe("canvas2d");
    expect(display.state.backend).toBe("canvas2d");
    expect(display.state.reason).toContain("WebGL1 is unavailable");
    expect(display.state.reason).toContain("2D fallback");
  });

  test("reports none, with both reasons, when there is no context of either kind", () => {
    const display = openDisplay(canvasWith(null, null), FRAME_WIDTH, FRAME_HEIGHT, undefined, () => null);
    expect(display.sink).toBeNull();
    expect(display.state.backend).toBe("none");
    expect(display.state.reason).toContain("no 2D context");
  });

  // The exact shader needs highp; at mediump it is silently off by one.
  describe("fragment precision", () => {
    const scratch = {
      createImageData: (w: number, h: number) => ({ width: w, height: h, data: new Uint8ClampedArray(w * h * 4) }),
      putImageData: () => {},
    };
    const scratchFactory = () => ({ canvas: {}, context: scratch });

    test("reads highp from the fragment stage's HIGH_FLOAT precision", () => {
      expect(hasFragmentHighp(recordingGl(23).gl)).toBe(true);
      expect(hasFragmentHighp(recordingGl(0).gl)).toBe(false);
    });

    test("goes to the 2D fallback, never binding WebGL, when the probe finds no highp", () => {
      const canvas = canvasWith(recordingGl().gl, {});
      const display = openDisplay(canvas, FRAME_WIDTH, FRAME_HEIGHT, undefined, scratchFactory, () => "mediump-only");
      expect(canvas.asked).toEqual(["2d"]);
      expect(display.sink?.backend).toBe("canvas2d");
      expect(display.state.reason).toContain(NO_HIGHP_REASON);
      expect(display.state.reason).toContain("2D fallback");
    });

    test("stays on exact WebGL1 when the probe finds highp", () => {
      const canvas = canvasWith(recordingGl().gl, {});
      const display = openDisplay(canvas, FRAME_WIDTH, FRAME_HEIGHT, undefined, scratchFactory, () => "highp");
      expect(canvas.asked).toEqual(["webgl"]);
      expect(display.state).toEqual({ backend: "webgl1", reason: null, contextLost: false });
    });

    test("names the inexactness when there was no probe and the bound context has no highp", () => {
      const canvas = canvasWith(recordingGl(0).gl, {});
      const display = openDisplay(canvas, FRAME_WIDTH, FRAME_HEIGHT, undefined, scratchFactory, () => "unknown");
      expect(display.sink?.backend).toBe("webgl1");
      expect(display.state.reason).toContain(NO_HIGHP_REASON);
      expect(display.state.reason).toContain("mediump");
    });
  });

  test("reports none rather than throwing when WebGL1 setup fails", () => {
    const { gl } = recordingGl();
    const broken = new Proxy(gl, {
      get: (target, key) => (key === "createTexture" ? () => null : Reflect.get(target, key)),
    });
    const display = openDisplay(canvasWith(broken, {}), FRAME_WIDTH, FRAME_HEIGHT);
    expect(display.sink).toBeNull();
    expect(display.state.reason).toBe("WebGL1 setup failed: WebGL gave no texture");
  });
});
