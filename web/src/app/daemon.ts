// Whether a `passportsim` daemon served this page, and so the page attaches to it. The daemon's
// session cookie is also the attach socket's credential (`crates/pemu-host/src/attach.rs`), so
// the page attaches only when served over `http` from `127.0.0.1` and the daemon's `/v1/health`
// answers with that cookie.

/** Where the page was loaded from; the fields of `Location` this module reads. */
export interface PageLocation {
  readonly protocol: string;
  readonly hostname: string;
  readonly port: string;
}

/** The `fetch` this module uses, narrowed so a test can answer it. */
export type HealthFetch = (
  path: string,
  init: { credentials: "same-origin"; cache: "no-store" },
) => Promise<{ ok: boolean; json(): Promise<unknown> }>;

/** The daemon port to attach to, or `null` when no daemon served the page. Never throws. */
export async function daemonAttachPort(location: PageLocation, fetchHealth: HealthFetch): Promise<number | null> {
  if (location.protocol !== "http:" || location.hostname !== "127.0.0.1") {
    return null;
  }
  const port = Number(location.port === "" ? "80" : location.port);
  if (!Number.isInteger(port) || port <= 0 || port > 65535) {
    return null;
  }
  try {
    const answer = await fetchHealth("/v1/health", { credentials: "same-origin", cache: "no-store" });
    if (!answer.ok) {
      return null;
    }
    const body = (await answer.json()) as { core?: unknown } | null;
    return body?.core === "pemu" ? port : null;
  } catch {
    return null;
  }
}
