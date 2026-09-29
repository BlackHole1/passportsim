// The header bar's model. Formatting lives here rather than in the view because Playwright reads
// these numbers back, and because virtual time is picoseconds while the header shows seconds.

import type { PacingMode } from "../worker/pacing";

export type LeaseOwner = "ui" | "agent" | "endpoint";

export interface HeaderModel {
  readonly product: "PassportSim";
  readonly image: string;
  /** A short id of the loaded build: the `elf_sha256` prefix `status` reports, `--` until then. */
  readonly buildId: string;
  /** Instance id, `p1` natively or `b1` when attached from a tab. */
  readonly instance: string;
  readonly virtualTime: string;
  readonly realTimeFactor: string;
  readonly lease: LeaseOwner;
  readonly inputDisabled: boolean;
  readonly banner: string | null;
  readonly running: boolean;
}

export interface HeaderInput {
  readonly image: string;
  readonly buildId: string;
  readonly instance: string;
  readonly nowPs: bigint;
  readonly realTimeFactor: number;
  readonly lease: LeaseOwner;
  readonly mode: PacingMode;
}

/**
 * Hex digits of the build hash shown: as many as ESP-IDF prints at start
 * (`CONFIG_APP_RETRIEVE_LEN_ELF_SHA`'s default), so the header and the console agree.
 */
export const BUILD_ID_DIGITS = 9;

export function buildIdOf(status: unknown): string | null {
  const instances = (status as { instances?: unknown } | null)?.instances;
  const first = Array.isArray(instances) ? (instances[0] as { build?: { elf_sha256?: unknown } } | undefined) : undefined;
  const sha = first?.build?.elf_sha256;
  return typeof sha === "string" && /^[0-9a-f]{64}$/.test(sha) ? sha.slice(0, BUILD_ID_DIGITS) : null;
}

const PS_PER_MS = 1_000_000_000n;

/** `vt 12.402 s`: always seconds with three decimals, so the width does not jump while it moves. */
export function formatVirtualTime(nowPs: bigint): string {
  const ms = nowPs / PS_PER_MS;
  const negative = ms < 0n;
  const abs = negative ? -ms : ms;
  const seconds = abs / 1000n;
  const millis = abs % 1000n;
  return `${negative ? "-" : ""}${seconds}.${millis.toString().padStart(3, "0")} s`;
}

/** `1.00x`, as measured: never clamped, so a slow or hidden tab shows below 1. */
export function formatRealTimeFactor(factor: number): string {
  if (!Number.isFinite(factor) || factor < 0) {
    return "--x";
  }
  return `${factor.toFixed(2)}x`;
}

/** The banner shown while an agent holds the clock, naming the virtual time it paused at. */
export function agentBanner(lease: LeaseOwner, nowPs: bigint): string | null {
  if (lease !== "agent") {
    return null;
  }
  const ms = nowPs / PS_PER_MS;
  const micros = (nowPs / 1_000_000n) % 1000n;
  return `controlled by agent, paused at ${ms}.${micros.toString().padStart(3, "0")} ms`;
}

export function headerModel(input: HeaderInput): HeaderModel {
  return {
    product: "PassportSim",
    image: input.image,
    buildId: input.buildId,
    instance: input.instance,
    virtualTime: formatVirtualTime(input.nowPs),
    realTimeFactor: formatRealTimeFactor(input.realTimeFactor),
    lease: input.lease,
    inputDisabled: input.lease === "agent",
    banner: agentBanner(input.lease, input.nowPs),
    running: input.mode.kind !== "Paused",
  };
}
