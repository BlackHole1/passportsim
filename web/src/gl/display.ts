// Picks the panel renderer for a transferred canvas and says which. WebGL1 first; without a WebGL1
// context the canvas can still give a `"2d"` one, so the panel falls back to `canvas2d.ts`, and the
// choice and reason go to the page. The exact shader needs `highp` fragment floats (at `mediump`,
// relative precision 2^-8, `floor(c8 * gain + 0.5)` is not exact), and a canvas bound to WebGL can
// no longer give a 2D context, so a throwaway 1x1 `OffscreenCanvas` is asked first.

import { Canvas2dRenderer, type ScratchContext, type TargetContext } from "./canvas2d";
import { GL_ATTRIBUTES, PanelRenderer, type Gl, type LossEvents } from "./renderer";
import type { DisplayState, PanelSink } from "./sink";

export interface DisplayCanvas extends LossEvents {
  getContext(kind: "webgl", attributes: WebGLContextAttributes): unknown;
  getContext(kind: "2d", attributes: { alpha: boolean }): unknown;
}

export type ScratchFactory = (
  width: number,
  height: number,
) => { readonly canvas: unknown; readonly context: ScratchContext } | null;

export type HighpProbe = () => "highp" | "mediump-only" | "unknown";

export function hasFragmentHighp(gl: Pick<Gl, "getShaderPrecisionFormat" | "FRAGMENT_SHADER" | "HIGH_FLOAT">): boolean {
  const format = gl.getShaderPrecisionFormat(gl.FRAGMENT_SHADER, gl.HIGH_FLOAT);
  return (format?.precision ?? 0) > 0;
}

export const offscreenHighpProbe: HighpProbe = () => {
  if (typeof OffscreenCanvas === "undefined") {
    return "unknown";
  }
  const gl = new OffscreenCanvas(1, 1).getContext("webgl") as Gl | null;
  if (!gl) {
    return "unknown";
  }
  const answer = hasFragmentHighp(gl) ? "highp" : "mediump-only";
  gl.getExtension("WEBGL_lose_context")?.loseContext();
  return answer;
};

export const NO_HIGHP_REASON =
  "WebGL1 fragment shaders have no highp float here, so the exact RGB565 shader cannot run";

export interface Display {
  readonly sink: PanelSink | null;
  readonly state: DisplayState;
}

export const offscreenScratch: ScratchFactory = (width, height) => {
  if (typeof OffscreenCanvas === "undefined") {
    return null;
  }
  const canvas = new OffscreenCanvas(width, height);
  const context = canvas.getContext("2d");
  return context ? { canvas, context: context as unknown as ScratchContext } : null;
};

/** Opens the best display `canvas` offers; `onState` hears later context losses and restores. */
export function openDisplay(
  canvas: DisplayCanvas,
  width: number,
  height: number,
  onState: (state: DisplayState) => void = () => {},
  scratch: ScratchFactory = offscreenScratch,
  probe: HighpProbe = offscreenHighpProbe,
): Display {
  if (probe() === "mediump-only") {
    return fallback(canvas, width, height, scratch, NO_HIGHP_REASON);
  }
  const gl = canvas.getContext("webgl", GL_ATTRIBUTES) as Gl | null;
  if (gl) {
    try {
      const sink = PanelRenderer.create(gl, width, height, canvas, onState);
      // Only without a usable probe; the canvas is WebGL now, so name the risk instead of falling back.
      const reason = hasFragmentHighp(gl)
        ? null
        : `${NO_HIGHP_REASON}; no probe canvas was available before this one was bound to WebGL, so the panel is drawn at mediump and may be off by one per channel`;
      return { sink, state: { backend: "webgl1", reason, contextLost: false } };
    } catch (error) {
      // The canvas is bound to WebGL now, so a 2D context can no longer be taken from it.
      const message = error instanceof Error ? error.message : String(error);
      return {
        sink: null,
        state: { backend: "none", reason: `WebGL1 setup failed: ${message}`, contextLost: false },
      };
    }
  }
  return fallback(canvas, width, height, scratch, 'WebGL1 is unavailable: getContext("webgl") returned null');
}

function fallback(
  canvas: DisplayCanvas,
  width: number,
  height: number,
  scratch: ScratchFactory,
  glFailure: string,
): Display {
  const target = canvas.getContext("2d", { alpha: false }) as TargetContext<unknown> | null;
  const store = target ? scratch(width, height) : null;
  if (!target || !store) {
    return {
      sink: null,
      state: {
        backend: "none",
        reason: target
          ? `${glFailure}, and no scratch canvas for the 2D fallback is available`
          : `${glFailure}, and no 2D context is available either`,
        contextLost: false,
      },
    };
  }
  return {
    sink: new Canvas2dRenderer(target, store.context, store.canvas, width, height),
    state: { backend: "canvas2d", reason: `${glFailure}; drawing with the 2D fallback`, contextLost: false },
  };
}
