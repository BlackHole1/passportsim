// The fill levels both ends of the playback transport agree on, in a module of their own so the
// pump need not import the worklet module, which registers processors on load.

/** The Web Audio 1.0 render quantum, fixed at 128 frames. */
export const RENDER_QUANTUM = 128;

/** Fill level the drift controller aims for: the 60 ms audio pacing lead. */
export const TARGET_FILL_MS = 60;

/** Below this fill the worklet outputs silence and counts an underrun. */
export const UNDERRUN_FILL_MS = 10;

/** Above this fill the worklet drops to the target and counts an overflow. */
export const OVERFLOW_FILL_MS = 250;
