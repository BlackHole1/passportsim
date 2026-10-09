// A play of FoloToy's AI Passport site as a firmware source: the page takes a play's link or its
// number, asks for the firmware of the play's published revision, downloads it and checks it
// against the size and SHA-256 the site stated. No DOM and an injected `fetch`, so every refusal is
// decided here and a test answers the site.
//
// The page never requests the play site itself: that site answers only its own pages, so a browser
// on any other origin is refused the read. It asks a relay on its own origin instead, at
// `PLAY_RELAY_PATH` beside the page (`edge/playRelay.js`, the Cloudflare Worker script of the web
// bundle), which passes two shapes of GET on to the play site. A pasted link contributes its play
// number and nothing else, and a firmware address the site returns is used only as a path on it.

import { DownloadReporter, readCounted, type DownloadListener } from "../worker/download";
import { FLASH_SIZE_BYTES, IMAGE_MAGIC } from "./load";

/** The site whose plays the page loads. */
export const PLAY_SITE = "https://ai-passport.folotoy.cn";

/** The host of {@link PLAY_SITE}, as the page names it. */
export const PLAY_HOST = new URL(PLAY_SITE).host;

/** The one firmware layout the site publishes that the page boots: a merged image for offset 0. */
export const PLAY_FORMAT = "esp-merged-0x0";

/** The relay's directory beside the page; `edge/playRelay.js` answers it as `RELAY_PREFIX`. */
export const PLAY_RELAY_PATH = "play-site";

/** The header every answer of the relay carries (`edge/playRelay.js` `RELAY_HEADER`). */
export const PLAY_RELAY_HEADER = "x-play-relay";

/** The firmware paths of the play site the relay passes on. */
const DOWNLOAD_PATH = /^\/api\/download(?:\/[A-Za-z0-9][A-Za-z0-9._-]{0,127}){1,3}$/;

/** The `fetch` this module uses, narrowed so a test can answer it. Never sent credentials. */
export type PlayFetch = (url: string, init: { credentials: "omit"; cache: "no-store" }) => Promise<Response>;

/** How the page reaches the play site: the relay's address, ending in `/`, and the `fetch` to ask it with. */
export interface PlayRelay {
  readonly base: string;
  readonly fetch: PlayFetch;
}

const REQUEST = { credentials: "omit", cache: "no-store" } as const;

/** The relay's address for a page whose script is at `scriptUrl`: `PLAY_RELAY_PATH` beside it. */
export function playRelayBase(scriptUrl: string): string {
  return new URL(`./${PLAY_RELAY_PATH}/`, scriptUrl).href;
}

export type PlayRefFault =
  /** Nothing was typed. */
  | { readonly kind: "empty" }
  /** A link to some other site; the page requests only the play site. */
  | { readonly kind: "other-site"; readonly host: string }
  /** Neither a play number nor a link to a play's page. */
  | { readonly kind: "not-a-play" };

export type PlayRef = { readonly ok: true; readonly id: number } | { readonly ok: false; readonly fault: PlayRefFault };

/** A play's page path, with the site's optional locale prefix. */
const PLAY_PATH = /^\/(?:(?:en|zh)\/)?plays\/([0-9]{1,9})\/?$/;

/**
 * The play a typed text names: its number (`1039`) or the address of its page
 * (`https://ai-passport.folotoy.cn/plays/1039/`, with or without the scheme, the locale prefix or
 * the trailing slash). Only the number is taken from a link.
 */
export function parsePlayRef(text: string): PlayRef {
  const raw = text.trim();
  if (raw === "") {
    return { ok: false, fault: { kind: "empty" } };
  }
  if (/^[0-9]{1,9}$/.test(raw)) {
    const id = Number(raw);
    return id > 0 ? { ok: true, id } : { ok: false, fault: { kind: "not-a-play" } };
  }
  // A link is what carries a scheme, or a dotted host name followed by a path; a bare word or a
  // decimal would otherwise parse as a host of its own.
  const schemed = /^[a-z][a-z0-9+.-]*:\/\//i.test(raw);
  if (!schemed && !/^[a-z0-9-]+(?:\.[a-z0-9-]+)+(?::[0-9]+)?\//i.test(raw)) {
    return { ok: false, fault: { kind: "not-a-play" } };
  }
  let url: URL;
  try {
    url = new URL(schemed ? raw : `https://${raw}`);
  } catch {
    return { ok: false, fault: { kind: "not-a-play" } };
  }
  if (url.protocol !== "https:" && url.protocol !== "http:") {
    return { ok: false, fault: { kind: "not-a-play" } };
  }
  if (url.host !== PLAY_HOST) {
    return { ok: false, fault: { kind: "other-site", host: url.host } };
  }
  const id = Number(PLAY_PATH.exec(url.pathname)?.[1] ?? "0");
  return id > 0 ? { ok: true, id } : { ok: false, fault: { kind: "not-a-play" } };
}

/** The address of a play's page, for a link the reader follows by hand. */
export function playPageUrl(id: number): string {
  return `${PLAY_SITE}/plays/${id}/`;
}

/** The firmware of a play's published revision, as the site's API states it. */
export interface PlayFirmware {
  readonly id: number;
  readonly revision: number;
  /** The play's title in the site's two languages; either may be empty. */
  readonly title: { readonly zh: string; readonly en: string };
  /** The firmware's path on the play site, which the relay passes on: `/api/download/...`. */
  readonly path: string;
  readonly size: number;
  /** Lower-case hex. */
  readonly sha256: string;
}

export type PlayFault =
  /** The request did not complete: the network, or the server of the page, is gone. */
  | { readonly kind: "unreachable"; readonly detail: string }
  /**
   * The server of the page answered without the relay's header: it has no relay. A static server
   * and the `passportsim` daemon serve the page's files and nothing else.
   */
  | { readonly kind: "no-relay" }
  | { readonly kind: "not-found"; readonly id: number }
  | { readonly kind: "http"; readonly status: number }
  /** The answer is not the play the API documents. */
  | { readonly kind: "malformed"; readonly detail: string }
  /** The play is published without a firmware to download. */
  | { readonly kind: "no-firmware"; readonly id: number }
  | { readonly kind: "format"; readonly format: string }
  | { readonly kind: "too-large"; readonly size: number }
  /** The downloaded bytes are not the ones the API described. */
  | { readonly kind: "size"; readonly stated: number; readonly received: number }
  | { readonly kind: "sha256"; readonly stated: string; readonly computed: string }
  | { readonly kind: "not-an-image" };

export type PlayResult<T> = { readonly ok: true; readonly value: T } | { readonly ok: false; readonly fault: PlayFault };

function fault<T>(of: PlayFault): PlayResult<T> {
  return { ok: false, fault: of };
}

function errorText(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function text(value: unknown): string {
  return typeof value === "string" ? value : "";
}

/** The firmware `body` (the API's answer for one play) describes, or why it describes none. */
export function playFirmware(id: number, body: unknown): PlayResult<PlayFirmware> {
  const play = (body as { play?: unknown } | null)?.play as Record<string, unknown> | null | undefined;
  if (typeof play !== "object" || play === null) {
    return fault({ kind: "malformed", detail: "the answer carries no `play`" });
  }
  const firmware = play.firmware as Record<string, unknown> | null | undefined;
  if (typeof firmware !== "object" || firmware === null || firmware.available !== true) {
    return fault({ kind: "no-firmware", id });
  }
  if (firmware.format !== PLAY_FORMAT) {
    return fault({ kind: "format", format: text(firmware.format) || "unnamed" });
  }
  const { size, sha256, url } = firmware;
  if (typeof size !== "number" || !Number.isSafeInteger(size) || size <= 0) {
    return fault({ kind: "malformed", detail: "the firmware has no size" });
  }
  if (size > FLASH_SIZE_BYTES) {
    return fault({ kind: "too-large", size });
  }
  if (typeof sha256 !== "string" || !/^[0-9a-fA-F]{64}$/.test(sha256)) {
    return fault({ kind: "malformed", detail: "the firmware has no SHA-256" });
  }
  let resolved: URL;
  try {
    resolved = new URL(text(url), `${PLAY_SITE}/`);
  } catch {
    return fault({ kind: "malformed", detail: "the firmware has no address" });
  }
  if (text(url) === "" || resolved.origin !== PLAY_SITE) {
    return fault({ kind: "malformed", detail: `the firmware is not served from ${PLAY_HOST}` });
  }
  if (!DOWNLOAD_PATH.test(resolved.pathname) || resolved.search !== "") {
    return fault({ kind: "malformed", detail: `the firmware is at \`${resolved.pathname}${resolved.search}\`, which is not a download of ${PLAY_HOST}` });
  }
  const revision = play.revisionId;
  const title = play.title as { zh?: unknown; en?: unknown } | null | undefined;
  return {
    ok: true,
    value: {
      id,
      revision: typeof revision === "number" && Number.isSafeInteger(revision) && revision >= 0 ? revision : 0,
      title: { zh: text(title?.zh), en: text(title?.en) },
      path: resolved.pathname,
      size,
      sha256: sha256.toLowerCase(),
    },
  };
}

/**
 * Asks the relay for `path` of the play site. An answer without the relay's header is not the
 * relay's, whatever its status: a static server's 404, or a page it serves for every path.
 */
async function ask(relay: PlayRelay, path: string, id: number): Promise<PlayResult<Response>> {
  let response: Response;
  try {
    response = await relay.fetch(`${relay.base}${path.replace(/^\/+/, "")}`, REQUEST);
  } catch (error) {
    return fault({ kind: "unreachable", detail: errorText(error) });
  }
  if (response.headers.get(PLAY_RELAY_HEADER) === null) {
    await response.body?.cancel();
    return fault({ kind: "no-relay" });
  }
  if (!response.ok) {
    await response.body?.cancel();
    return fault(response.status === 404 ? { kind: "not-found", id } : { kind: "http", status: response.status });
  }
  return { ok: true, value: response };
}

/** Asks the play site, through the relay, which firmware play `id` publishes. */
export async function resolvePlay(id: number, relay: PlayRelay): Promise<PlayResult<PlayFirmware>> {
  const asked = await ask(relay, `/api/plays/id/${id}`, id);
  if (!asked.ok) {
    return asked;
  }
  const response = asked.value;
  let body: unknown;
  try {
    body = await response.json();
  } catch {
    return fault({ kind: "malformed", detail: "the answer is not JSON" });
  }
  return playFirmware(id, body);
}

async function sha256Hex(bytes: Uint8Array): Promise<string> {
  const digest = await crypto.subtle.digest("SHA-256", bytes as Uint8Array<ArrayBuffer>);
  return [...new Uint8Array(digest)].map((byte) => byte.toString(16).padStart(2, "0")).join("");
}

/**
 * Downloads a play's firmware and accepts it only as the bytes the API described: the stated
 * size, the stated SHA-256, and an ESP image's first byte. The same three checks the site makes
 * before it flashes a device.
 */
export async function downloadPlay(
  firmware: PlayFirmware,
  relay: PlayRelay,
  onDownload: DownloadListener,
  nowMs: () => number,
): Promise<PlayResult<Uint8Array>> {
  const reporter = new DownloadReporter("play", onDownload, nowMs);
  reporter.start();
  const asked = await ask(relay, firmware.path, firmware.id);
  if (!asked.ok) {
    reporter.fail(asked.fault.kind === "unreachable" ? asked.fault.detail : asked.fault.kind === "http" ? `HTTP ${asked.fault.status}` : asked.fault.kind);
    return asked;
  }
  let bytes: Uint8Array;
  try {
    bytes = await readCounted(asked.value, reporter);
  } catch (error) {
    reporter.fail(error);
    return fault({ kind: "unreachable", detail: errorText(error) });
  }
  if (bytes.length !== firmware.size) {
    return fault({ kind: "size", stated: firmware.size, received: bytes.length });
  }
  const computed = await sha256Hex(bytes);
  if (computed !== firmware.sha256) {
    return fault({ kind: "sha256", stated: firmware.sha256, computed });
  }
  if (bytes[0] !== IMAGE_MAGIC) {
    return fault({ kind: "not-an-image" });
  }
  return { ok: true, value: bytes };
}

/**
 * The name a play's firmware runs under: the header, the history and a screenshot's file name
 * carry it, so it is the play and its revision in plain ASCII and never the site's title.
 */
export function playImageName(firmware: Pick<PlayFirmware, "id" | "revision">): string {
  return firmware.revision > 0 ? `play-${firmware.id}-r${firmware.revision}` : `play-${firmware.id}`;
}

/** The title to show beside the name: the reader's language when the play has it, else the other. */
export function playTitle(firmware: Pick<PlayFirmware, "title">, chinese: boolean): string {
  const { zh, en } = firmware.title;
  return (chinese ? zh || en : en || zh).trim();
}
