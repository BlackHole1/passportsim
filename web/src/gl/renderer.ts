// The OffscreenCanvas WebGL1 panel renderer.
//
// Upload: an `RGB`/`UNSIGNED_SHORT_5_6_5` texture filled straight from the wasm `Uint16Array` with
// `texSubImage2D`, no per-pixel JS. In Chromium on SwiftShader a full frame took about 0.075 ms,
// against 0.126 ms for a CPU LUT unpack plus RGBA upload; all far inside a 16 ms present.
//
// Exactness: a GPU maps a 5-bit code `c` to `c / 31`, which is one off the bit replication of
// `rgb565.ts` for 14 codes, so the shader rebuilds the integer code, applies the glass complement,
// bit replication and backlight in integer steps, and writes `k / 255`, which an 8-bit framebuffer
// stores exactly. `highp` is requested where the fragment stage has it.
//
// Context loss: `webglcontextlost` is `preventDefault`ed (or the browser never restores), and on
// `webglcontextrestored` the program, buffer and texture are rebuilt and the last frame redrawn,
// so a restored panel is not black until the guest repaints.

import { RowViews } from "./dirty";
import { panelGain, type PanelView } from "./rgb565";
import type { DisplayState, PanelSink } from "./sink";
import { ViewportCache, type Viewport } from "./viewport";

const VERTEX_SHADER = `
attribute vec2 a_pos;
varying vec2 v_uv;
void main() {
  v_uv = vec2(a_pos.x * 0.5 + 0.5, 0.5 - a_pos.y * 0.5);
  gl_Position = vec4(a_pos, 0.0, 1.0);
}`;

/** Exported so a test can check the arithmetic it spells against `rgb565.ts`. */
export const FRAGMENT_SHADER = `
#ifdef GL_FRAGMENT_PRECISION_HIGH
precision highp float;
#else
precision mediump float;
#endif
varying vec2 v_uv;
uniform sampler2D u_panel;
uniform float u_gain;
uniform float u_invert;
void main() {
  vec3 top = vec3(31.0, 63.0, 31.0);
  vec3 code = floor(texture2D(u_panel, v_uv).rgb * top + 0.5);
  code = mix(code, top - code, u_invert);
  vec3 c8 = code * vec3(8.0, 4.0, 8.0) + floor(code / vec3(4.0, 16.0, 4.0));
  gl_FragColor = vec4(floor(c8 * u_gain + 0.5) / 255.0, 1.0);
}`;

export type Gl = WebGLRenderingContext;

export interface LossEvents {
  addEventListener(type: string, listener: (event: Event) => void): void;
}

interface GlResources {
  readonly program: WebGLProgram;
  readonly texture: WebGLTexture;
  readonly gain: WebGLUniformLocation | null;
  readonly invert: WebGLUniformLocation | null;
}

/**
 * Uploads RGB565 rows from wasm memory and draws the panel at an integer scale. The texture is
 * allocated once; steady state allocates nothing.
 */
export class PanelRenderer implements PanelSink {
  readonly backend = "webgl1" as const;
  private resources: GlResources | null = null;
  private readonly rowViews: RowViews;
  private readonly viewports: ViewportCache;
  private uploadedRows = 0;
  private lost = false;
  private uploadAllNext = false;
  /**
   * Set by such a restore: the texture holds zeros, which are not black under the glass complement,
   * so draws clear to black until a whole frame is uploaded.
   */
  private awaitingFullUpload = false;
  private lastPixels: Uint16Array | null = null;
  private lastPanel: PanelView | null = null;
  private lastCanvasWidth = 0;
  private lastCanvasHeight = 0;

  private constructor(
    private readonly gl: Gl,
    private readonly width: number,
    private readonly height: number,
    private readonly onState: (state: DisplayState) => void,
  ) {
    this.rowViews = new RowViews(width, height);
    this.viewports = new ViewportCache(width, height);
  }

  static create(
    gl: Gl,
    width: number,
    height: number,
    events?: LossEvents,
    onState: (state: DisplayState) => void = () => {},
  ): PanelRenderer {
    const renderer = new PanelRenderer(gl, width, height, onState);
    renderer.resources = createResources(gl, width, height);
    events?.addEventListener("webglcontextlost", (event) => {
      event.preventDefault();
      renderer.contextLost();
    });
    events?.addEventListener("webglcontextrestored", () => {
      renderer.contextRestored();
    });
    return renderer;
  }

  get rowsUploaded(): number {
    return this.uploadedRows;
  }

  get isLost(): boolean {
    return this.lost;
  }

  upload(pixels: Uint16Array, first: number, last: number): void {
    this.lastPixels = pixels;
    if (this.lost || !this.resources) {
      return;
    }
    if (pixels.length < this.width * this.height) {
      // A detached or short view: send everything once it is whole again.
      this.uploadAllNext = true;
      return;
    }
    let from = Math.max(0, first);
    let to = Math.min(this.height - 1, last);
    if (this.uploadAllNext) {
      from = 0;
      to = this.height - 1;
      this.uploadAllNext = false;
      this.awaitingFullUpload = false;
    }
    if (to < from) {
      return;
    }
    const rows = to - from + 1;
    const gl = this.gl;
    gl.bindTexture(gl.TEXTURE_2D, this.resources.texture);
    gl.texSubImage2D(
      gl.TEXTURE_2D,
      0,
      0,
      from,
      this.width,
      rows,
      gl.RGB,
      gl.UNSIGNED_SHORT_5_6_5,
      this.rowViews.from(pixels, from),
    );
    this.uploadedRows += rows;
  }

  draw(panel: PanelView, canvasWidth: number, canvasHeight: number): Viewport {
    this.lastPanel = panel;
    this.lastCanvasWidth = canvasWidth;
    this.lastCanvasHeight = canvasHeight;
    const viewport = this.viewports.get(canvasWidth, canvasHeight);
    const resources = this.resources;
    if (this.lost || !resources) {
      return viewport;
    }
    const gl = this.gl;
    // The area around an integer-scaled panel is black, not whatever the last frame left there.
    gl.viewport(0, 0, canvasWidth, canvasHeight);
    gl.clearColor(0, 0, 0, 1);
    gl.clear(gl.COLOR_BUFFER_BIT);
    if (this.awaitingFullUpload) {
      return viewport;
    }
    gl.useProgram(resources.program);
    gl.bindTexture(gl.TEXTURE_2D, resources.texture);
    gl.uniform1f(resources.gain, panelGain(panel));
    gl.uniform1f(resources.invert, panel.glassComplement ? 1 : 0);
    gl.viewport(viewport.x, viewport.glY, viewport.width, viewport.height);
    gl.drawArrays(gl.TRIANGLES, 0, 6);
    return viewport;
  }

  contextLost(): void {
    this.lost = true;
    this.resources = null;
    this.onState({ backend: "webgl1", reason: null, contextLost: true });
  }

  /**
   * The context is back, empty: rebuild and redraw the last frame. If its view was detached, clear to
   * black instead and send every row on the next upload.
   */
  contextRestored(): void {
    try {
      this.resources = createResources(this.gl, this.width, this.height);
    } catch {
      this.contextLost();
      return;
    }
    this.lost = false;
    this.uploadAllNext = true;
    const pixels = this.lastPixels;
    if (pixels) {
      this.upload(pixels, 0, this.height - 1);
    }
    this.awaitingFullUpload = this.uploadAllNext;
    if (this.lastPanel) {
      this.draw(this.lastPanel, this.lastCanvasWidth, this.lastCanvasHeight);
    }
    this.onState({ backend: "webgl1", reason: null, contextLost: false });
  }
}

function createResources(gl: Gl, width: number, height: number): GlResources {
  const program = linkProgram(gl, VERTEX_SHADER, FRAGMENT_SHADER);
  gl.useProgram(program);

  const quad = gl.createBuffer();
  if (!quad) {
    throw new Error("WebGL gave no vertex buffer");
  }
  gl.bindBuffer(gl.ARRAY_BUFFER, quad);
  gl.bufferData(
    gl.ARRAY_BUFFER,
    new Float32Array([-1, -1, 1, -1, -1, 1, -1, 1, 1, -1, 1, 1]),
    gl.STATIC_DRAW,
  );
  const position = gl.getAttribLocation(program, "a_pos");
  gl.enableVertexAttribArray(position);
  gl.vertexAttribPointer(position, 2, gl.FLOAT, false, 0, 0);

  const texture = gl.createTexture();
  if (!texture) {
    throw new Error("WebGL gave no texture");
  }
  gl.bindTexture(gl.TEXTURE_2D, texture);
  // NEAREST: the panel is shown at an integer scale, so filtering would only blur.
  gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, gl.NEAREST);
  gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, gl.NEAREST);
  gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE);
  gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE);
  gl.pixelStorei(gl.UNPACK_ALIGNMENT, 2);
  gl.pixelStorei(gl.UNPACK_FLIP_Y_WEBGL, false);
  gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGB, width, height, 0, gl.RGB, gl.UNSIGNED_SHORT_5_6_5, null);
  return {
    program,
    texture,
    gain: gl.getUniformLocation(program, "u_gain"),
    invert: gl.getUniformLocation(program, "u_invert"),
  };
}

function linkProgram(gl: Gl, vertex: string, fragment: string): WebGLProgram {
  const program = gl.createProgram();
  if (!program) {
    throw new Error("WebGL gave no program");
  }
  gl.attachShader(program, compile(gl, gl.VERTEX_SHADER, vertex));
  gl.attachShader(program, compile(gl, gl.FRAGMENT_SHADER, fragment));
  gl.linkProgram(program);
  if (!gl.getProgramParameter(program, gl.LINK_STATUS)) {
    throw new Error(`the panel program did not link: ${gl.getProgramInfoLog(program) ?? ""}`);
  }
  return program;
}

function compile(gl: Gl, kind: number, source: string): WebGLShader {
  const shader = gl.createShader(kind);
  if (!shader) {
    throw new Error("WebGL gave no shader");
  }
  gl.shaderSource(shader, source);
  gl.compileShader(shader);
  if (!gl.getShaderParameter(shader, gl.COMPILE_STATUS)) {
    throw new Error(`a panel shader did not compile: ${gl.getShaderInfoLog(shader) ?? ""}`);
  }
  return shader;
}

export const GL_ATTRIBUTES: WebGLContextAttributes = {
  alpha: false,
  antialias: false,
  depth: false,
  stencil: false,
  preserveDrawingBuffer: false,
};
