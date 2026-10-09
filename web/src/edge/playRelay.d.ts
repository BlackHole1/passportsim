// The types of `playRelay.js`, which is plain JavaScript because it ships as written. Everything
// is on the default export, since a Worker script may have no other.

export type UpstreamFetch = (url: string, init: { method: "GET"; redirect: "manual"; headers: Record<string, string> }) => Promise<Response>;

declare const worker: {
  fetch(request: Request): Promise<Response>;
  /** The relay's answer, or `null` for a path outside its prefix. */
  relay(request: Request, fetchUpstream?: UpstreamFetch): Promise<Response | null>;
  upstreamUrl(pathname: string): string | null;
  readonly UPSTREAM: string;
  readonly RELAY_PREFIX: string;
  readonly RELAY_HEADER: string;
  readonly MAX_FIRMWARE_BYTES: number;
};
export default worker;
