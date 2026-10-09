// The relay is a fake `fetch` here: its answers carry the relay's header and have the shape the
// play site's `GET /api/plays/id/<id>` gave on 2026-10-08, cut down to the fields the page reads,
// and the firmware is a synthetic merged bin.

import { describe, expect, test } from "bun:test";
import type { DownloadProgress } from "../worker/download";
import { translator } from "./i18n";
import { FLASH_SIZE_BYTES, IMAGE_MAGIC, type LoadedImage } from "./load";
import { createLoader } from "./loader";
import {
  downloadPlay,
  parsePlayRef,
  PLAY_RELAY_HEADER,
  playFirmware,
  playImageName,
  playPageUrl,
  playRelayBase,
  playTitle,
  resolvePlay,
  type PlayFirmware,
  type PlayRelay,
} from "./play";
import { loaderText } from "./view/Firmware";
import { stepText } from "./view/Log";

const t = translator("en");

function mergedBin(length = 0x20_000): Uint8Array {
  const bytes = new Uint8Array(length);
  bytes[0] = IMAGE_MAGIC;
  bytes[length - 1] = 0x5a;
  return bytes;
}

async function sha256(bytes: Uint8Array): Promise<string> {
  const digest = await crypto.subtle.digest("SHA-256", bytes as Uint8Array<ArrayBuffer>);
  return [...new Uint8Array(digest)].map((byte) => byte.toString(16).padStart(2, "0")).join("");
}

function answer(firmware: Record<string, unknown> | null, more: Record<string, unknown> = {}): unknown {
  return {
    ok: true,
    play: {
      id: 1039,
      revisionId: 2347,
      slug: "community-b6eb4756",
      title: { zh: "口袋天气", en: "Pocket Weather" },
      ...(firmware === null ? {} : { firmware }),
      ...more,
    },
  };
}

async function described(bytes: Uint8Array, more: Record<string, unknown> = {}): Promise<Record<string, unknown>> {
  return {
    available: true,
    size: bytes.length,
    sha256: await sha256(bytes),
    url: "/api/download/community/community-b6eb4756",
    format: "esp-merged-0x0",
    ...more,
  };
}

const RELAY = "https://passportsim.test/play-site/";
const DOWNLOAD = `${RELAY}api/download/community/community-b6eb4756`;

function json(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), { status, headers: { "content-type": "application/json", [PLAY_RELAY_HEADER]: "1" } });
}

function binary(bytes: Uint8Array, status = 200): Response {
  return new Response(bytes as Uint8Array<ArrayBuffer>, { status, headers: { "content-length": String(bytes.length), [PLAY_RELAY_HEADER]: "1" } });
}

/** A relay that answers the API with `api` and the download with `file`, recording each request. */
function site(api: () => Response | Promise<Response>, file: () => Response | Promise<Response> = () => json({ detail: "unused" }, 502)) {
  const requests: { url: string; init: unknown }[] = [];
  const relay: PlayRelay = {
    base: RELAY,
    fetch: async (url, init) => {
      requests.push({ url, init });
      return url.includes("/api/plays/id/") ? api() : file();
    },
  };
  return { relay, requests };
}

function failing(error: Error): PlayRelay {
  return { base: RELAY, fetch: () => Promise.reject(error) };
}

describe("parsePlayRef", () => {
  test("takes a play's number", () => {
    expect(parsePlayRef("1039")).toEqual({ ok: true, id: 1039 });
    expect(parsePlayRef("  1039\n")).toEqual({ ok: true, id: 1039 });
  });

  test("takes the address of a play's page, with or without the scheme, locale, slash, query or fragment", () => {
    for (const text of [
      "https://ai-passport.folotoy.cn/plays/1039/",
      "https://ai-passport.folotoy.cn/plays/1039",
      "https://ai-passport.folotoy.cn/en/plays/1039/",
      "https://ai-passport.folotoy.cn/zh/plays/1039/",
      "http://ai-passport.folotoy.cn/plays/1039/",
      "ai-passport.folotoy.cn/plays/1039/",
      "https://ai-passport.folotoy.cn/plays/1039/?from=share#comments",
      "HTTPS://AI-PASSPORT.FOLOTOY.CN/plays/1039/",
    ]) {
      expect(parsePlayRef(text), text).toEqual({ ok: true, id: 1039 });
    }
  });

  test("refuses a link to any other site by naming it, and never takes a number from it", () => {
    expect(parsePlayRef("https://example.com/plays/1039/")).toEqual({ ok: false, fault: { kind: "other-site", host: "example.com" } });
    expect(parsePlayRef("https://ai-passport.folotoy.cn.example.com/plays/1039/")).toEqual({
      ok: false,
      fault: { kind: "other-site", host: "ai-passport.folotoy.cn.example.com" },
    });
    expect(parsePlayRef("https://example.com/?next=https://ai-passport.folotoy.cn/plays/1039/")).toEqual({
      ok: false,
      fault: { kind: "other-site", host: "example.com" },
    });
    expect(parsePlayRef("https://ai-passport.folotoy.cn@example.com/plays/1039/")).toEqual({
      ok: false,
      fault: { kind: "other-site", host: "example.com" },
    });
  });

  test("refuses what is neither a number nor a play's page", () => {
    for (const text of [
      "0",
      "-3",
      "10.5",
      "12345678901",
      "pocket weather",
      "weather",
      "https://ai-passport.folotoy.cn/",
      "https://ai-passport.folotoy.cn/plays/",
      "https://ai-passport.folotoy.cn/plays/community/",
      "https://ai-passport.folotoy.cn/plays/1039/../../api/session",
      "https://ai-passport.folotoy.cn/api/plays/id/1039",
      "javascript:alert(1)",
      "file:///plays/1039/",
    ]) {
      expect(parsePlayRef(text), text).toEqual({ ok: false, fault: { kind: "not-a-play" } });
    }
    expect(parsePlayRef("   ")).toEqual({ ok: false, fault: { kind: "empty" } });
  });

  test("a play's page is the address the page links to", () => {
    expect(playPageUrl(1039)).toBe("https://ai-passport.folotoy.cn/plays/1039/");
    expect(parsePlayRef(playPageUrl(1039))).toEqual({ ok: true, id: 1039 });
  });
});

describe("playFirmware", () => {
  const firmware = { available: true, size: 2_041_808, sha256: "AB".repeat(32), url: "/api/download/community/community-b6eb4756", format: "esp-merged-0x0" };

  test("reads the published revision's firmware and keeps its address as a path on the play site", () => {
    expect(playFirmware(1039, answer(firmware))).toEqual({
      ok: true,
      value: {
        id: 1039,
        revision: 2347,
        title: { zh: "口袋天气", en: "Pocket Weather" },
        path: "/api/download/community/community-b6eb4756",
        size: 2_041_808,
        sha256: "ab".repeat(32),
      },
    });
  });

  test("refuses a firmware served from anywhere but the play site's downloads", () => {
    for (const url of [
      "https://example.com/api/download/community/fw",
      "//example.com/api/download/community/fw",
      "http://ai-passport.folotoy.cn/api/download/community/fw",
      "",
      "/api/session",
      "/api/download/",
      "/api/download/community/fw?next=/api/session",
      "/api/download/community/../../session",
      "/api/download/a/b/c/d",
    ]) {
      const result = playFirmware(1039, answer({ ...firmware, url }));
      expect(result.ok, url).toBe(false);
      expect(!result.ok && result.fault.kind, url).toBe("malformed");
    }
  });

  test("says which of the play's fields keeps it from loading", () => {
    expect(playFirmware(7, answer(null))).toEqual({ ok: false, fault: { kind: "no-firmware", id: 7 } });
    expect(playFirmware(7, answer({ ...firmware, available: false }))).toEqual({ ok: false, fault: { kind: "no-firmware", id: 7 } });
    expect(playFirmware(7, answer({ ...firmware, format: "esp-app-0x10000" }))).toEqual({ ok: false, fault: { kind: "format", format: "esp-app-0x10000" } });
    expect(playFirmware(7, answer({ ...firmware, size: FLASH_SIZE_BYTES + 1 }))).toEqual({ ok: false, fault: { kind: "too-large", size: FLASH_SIZE_BYTES + 1 } });
    for (const broken of [{ size: 0 }, { size: "2041808" }, { sha256: "fd8a" }, { sha256: null }]) {
      const result = playFirmware(7, answer({ ...firmware, ...broken }));
      expect(!result.ok && result.fault.kind, JSON.stringify(broken)).toBe("malformed");
    }
    for (const body of [null, {}, { ok: true }, { play: "1039" }, "play"]) {
      const result = playFirmware(7, body);
      expect(!result.ok && result.fault.kind, JSON.stringify(body)).toBe("malformed");
    }
  });

  test("names the image by play and revision, and picks the title by the reader's language", () => {
    expect(playImageName({ id: 1039, revision: 2347 })).toBe("play-1039-r2347");
    expect(playImageName({ id: 1039, revision: 0 })).toBe("play-1039");
    expect(playTitle({ title: { zh: "口袋天气", en: "Pocket Weather" } }, true)).toBe("口袋天气");
    expect(playTitle({ title: { zh: "口袋天气", en: "Pocket Weather" } }, false)).toBe("Pocket Weather");
    expect(playTitle({ title: { zh: "口袋天气", en: "" } }, false)).toBe("口袋天气");
    expect(playTitle({ title: { zh: "", en: "" } }, true)).toBe("");
  });
});

describe("resolvePlay", () => {
  test("asks the relay on the page's own origin for the play, without credentials or the cache", async () => {
    const bytes = mergedBin();
    const { relay, requests } = site(async () => json(answer(await described(bytes))));
    const result = await resolvePlay(1039, relay);
    expect(result.ok).toBe(true);
    expect(requests).toEqual([{ url: "https://passportsim.test/play-site/api/plays/id/1039", init: { credentials: "omit", cache: "no-store" } }]);
  });

  test("the relay is beside the page's script, wherever the bundle is served", () => {
    expect(playRelayBase("https://passportsim.bugs.cc/main.js")).toBe("https://passportsim.bugs.cc/play-site/");
    expect(playRelayBase("http://127.0.0.1:4173/emu/main.js")).toBe("http://127.0.0.1:4173/emu/play-site/");
  });

  test("a play the site does not have, an error status and a body that is not JSON are each named", async () => {
    expect(await resolvePlay(999_999, site(() => json({ detail: "no such play" }, 404)).relay)).toEqual({ ok: false, fault: { kind: "not-found", id: 999_999 } });
    expect(await resolvePlay(1039, site(() => json({ detail: "busy" }, 502)).relay)).toEqual({ ok: false, fault: { kind: "http", status: 502 } });
    expect(await resolvePlay(1039, site(() => new Response("<html>", { status: 200, headers: { [PLAY_RELAY_HEADER]: "1" } })).relay)).toEqual({
      ok: false,
      fault: { kind: "malformed", detail: "the answer is not JSON" },
    });
  });

  test("an answer without the relay's header is a server with no relay, whatever its status", async () => {
    for (const response of [
      () => new Response("not found", { status: 404 }),
      () => new Response("<!doctype html>", { status: 200, headers: { "content-type": "text/html" } }),
      () => new Response(JSON.stringify({ ok: true, play: {} }), { status: 200 }),
    ]) {
      expect(await resolvePlay(1039, site(response).relay)).toEqual({ ok: false, fault: { kind: "no-relay" } });
    }
  });

  test("a request that does not complete is unreachable, with the browser's words", async () => {
    expect(await resolvePlay(1039, failing(new TypeError("Failed to fetch")))).toEqual({ ok: false, fault: { kind: "unreachable", detail: "Failed to fetch" } });
  });
});

describe("downloadPlay", () => {
  async function firmwareOf(bytes: Uint8Array, more: Partial<PlayFirmware> = {}): Promise<PlayFirmware> {
    return {
      id: 1039,
      revision: 2347,
      title: { zh: "", en: "" },
      path: "/api/download/community/community-b6eb4756",
      size: bytes.length,
      sha256: await sha256(bytes),
      ...more,
    };
  }

  test("returns the bytes the site described and counts them as they arrive", async () => {
    const bytes = mergedBin();
    const reports: DownloadProgress[] = [];
    const { relay, requests } = site(() => json({}), () => binary(bytes));
    const result = await downloadPlay(await firmwareOf(bytes), relay, (report) => reports.push(report), () => 0);
    expect(result.ok && result.value.length).toBe(bytes.length);
    expect(requests).toEqual([{ url: DOWNLOAD, init: { credentials: "omit", cache: "no-store" } }]);
    expect(reports[0]).toEqual({ what: "play", received: 0, total: null, done: false });
    expect(reports.at(-1)).toEqual({ what: "play", received: bytes.length, total: bytes.length, done: true });
  });

  test("refuses a download of another size, of another digest, or that is not an ESP image", async () => {
    const bytes = mergedBin();
    const serve = site(() => json({}), () => binary(bytes)).relay;
    const quiet = () => {};
    expect(await downloadPlay(await firmwareOf(bytes, { size: bytes.length + 1 }), serve, quiet, () => 0)).toEqual({
      ok: false,
      fault: { kind: "size", stated: bytes.length + 1, received: bytes.length },
    });
    const other = "0".repeat(64);
    expect(await downloadPlay(await firmwareOf(bytes, { sha256: other }), serve, quiet, () => 0)).toEqual({
      ok: false,
      fault: { kind: "sha256", stated: other, computed: await sha256(bytes) },
    });
    const text = new TextEncoder().encode("<html>not a firmware</html>");
    expect(await downloadPlay(await firmwareOf(text), site(() => json({}), () => binary(text)).relay, quiet, () => 0)).toEqual({
      ok: false,
      fault: { kind: "not-an-image" },
    });
  });

  test("a refused or dropped download ends its progress line as failed", async () => {
    const bytes = mergedBin();
    const reports: DownloadProgress[] = [];
    const gone = await downloadPlay(await firmwareOf(bytes), site(() => json({}), () => json({ detail: "gone" }, 502)).relay, (report) => reports.push(report), () => 0);
    expect(gone).toEqual({ ok: false, fault: { kind: "http", status: 502 } });
    expect(reports.at(-1)).toMatchObject({ what: "play", done: true, error: "HTTP 502" });
    const dropped = await downloadPlay(await firmwareOf(bytes), failing(new TypeError("Load failed")), (report) => reports.push(report), () => 0);
    expect(dropped).toEqual({ ok: false, fault: { kind: "unreachable", detail: "Load failed" } });
    expect(reports.at(-1)).toMatchObject({ what: "play", done: true, error: "Load failed" });
  });
});

describe("the loader's play", () => {
  function loaderFor(playRelay: PlayRelay | undefined, onImage: (image: LoadedImage) => Promise<void> = () => Promise.resolve()) {
    const seen: LoadedImage[] = [];
    let clock = 1_000;
    const loader = createLoader(
      {
        onImage: async (image) => {
          seen.push(image);
          await onImage(image);
        },
        onDemo: () => Promise.resolve(),
        now: () => (clock += 200),
        ...(playRelay === undefined ? {} : { playRelay }),
      },
      "official",
    );
    return {
      loader,
      seen,
      state: () => loader.store.get().state,
      message: () => loaderText(t, loader.store.get().message),
      steps: () => loader.store.get().steps.map((line) => stepText(t, line.step)),
    };
  }

  test("a play's link boots its firmware as a merged bin named by play and revision", async () => {
    const bytes = mergedBin();
    const { relay, requests } = site(async () => json(answer(await described(bytes))), () => binary(bytes));
    const booted: LoadedImage[] = [];
    const page = loaderFor(relay);
    page.loader.store.subscribe(() => {});
    await page.loader.play("https://ai-passport.folotoy.cn/plays/1039/", true);
    booted.push(...page.seen);
    expect(requests.map((one) => one.url)).toEqual(["https://passportsim.test/play-site/api/plays/id/1039", DOWNLOAD]);
    expect(booted.length).toBe(1);
    expect(booted[0]?.name).toBe("play-1039-r2347");
    expect(booted[0]?.assets.map((asset) => [asset.file, asset.bytes.length])).toEqual([["play-1039-r2347.bin", bytes.length]]);
    expect(page.state()).toBe("loaded");
    expect(page.loader.store.get().image).toBe("play-1039-r2347");
    expect(page.message()).toBe('running play-1039-r2347: play 1039 "口袋天气" from ai-passport.folotoy.cn; flash play-1039-r2347.bin (128.0 KiB)');
    expect(page.steps()).toEqual([
      "Asking ai-passport.folotoy.cn for play 1039",
      "Play 1039, 口袋天气: revision 2347, 128.0 KiB",
      "Downloaded the play's firmware (131.1 kB)",
      `The download has the size and the SHA-256 the site states (${(await sha256(bytes)).slice(0, 12)})`,
      "Received 1 file(s), 128.0 KiB in total",
      "Read play-1039-r2347.bin (128.0 KiB)",
      "Detected a merged flash image",
    ]);
  });

  test("a bare number loads the same play, and the English title is shown to a reader who is not Chinese", async () => {
    const bytes = mergedBin();
    const page = loaderFor(site(async () => json(answer(await described(bytes))), () => binary(bytes)).relay);
    await page.loader.play("1039");
    expect(page.seen.map((image) => image.name)).toEqual(["play-1039-r2347"]);
    expect(page.message()).toContain('play 1039 "Pocket Weather" from ai-passport.folotoy.cn');
  });

  test("text that names no play is refused before any request", async () => {
    const { relay, requests } = site(() => json({}));
    const page = loaderFor(relay);
    await page.loader.play("https://example.com/plays/1039/");
    expect(page.state()).toBe("refused");
    expect(page.message()).toBe("`example.com` is not ai-passport.folotoy.cn: the page loads plays from ai-passport.folotoy.cn only");
    await page.loader.play("");
    expect(page.message()).toBe("type a play's number or the address of its page on ai-passport.folotoy.cn");
    await page.loader.play("weather");
    expect(page.message()).toBe("that is neither a play's number nor the address of a play's page, such as https://ai-passport.folotoy.cn/plays/22/");
    expect(requests).toEqual([]);
    expect(page.seen).toEqual([]);
  });

  test("a server with no relay is refused in words that say so and what to do", async () => {
    const page = loaderFor(site(() => new Response("not found", { status: 404 })).relay);
    await page.loader.play("1039");
    expect(page.state()).toBe("refused");
    expect(page.loader.store.get().message).toEqual({ kind: "play-fault", id: 1039, fault: { kind: "no-relay" } });
    expect(page.message()).toBe(
      "this server cannot load a play directly from ai-passport.folotoy.cn. Download the firmware from the play's page and drop it here.",
    );
    expect(page.steps()).toEqual(["Asking ai-passport.folotoy.cn for play 1039", "Stopped"]);
    expect(page.seen).toEqual([]);
  });

  test("a request that does not complete, and a relay that could not get the play, are each said", async () => {
    const offline = loaderFor(failing(new TypeError("Failed to fetch")));
    await offline.loader.play("1039");
    expect(offline.message()).toBe(
      "the request for play 1039 did not complete (Failed to fetch). Download the firmware from the play's page and drop it here.",
    );
    const down = loaderFor(site(() => json({ detail: "ai-passport.folotoy.cn answered HTTP 503" }, 502)).relay);
    await down.loader.play("1039");
    expect(down.message()).toBe("play 1039 could not be fetched from ai-passport.folotoy.cn (HTTP 502)");
    expect([offline.state(), down.state(), ...offline.seen, ...down.seen]).toEqual(["refused", "refused"]);
  });

  test("a download that does not match what the site stated never reaches the machine", async () => {
    const bytes = mergedBin();
    const tampered = mergedBin();
    tampered[100] = 0xff;
    const page = loaderFor(site(async () => json(answer(await described(bytes))), () => binary(tampered)).relay);
    await page.loader.play("1039");
    expect(page.state()).toBe("refused");
    expect(page.message()).toBe(
      `the download's SHA-256 is ${await sha256(tampered)} and ai-passport.folotoy.cn stated ${await sha256(bytes)}, so it was not loaded`,
    );
    expect(page.seen).toEqual([]);
  });

  test("a play with no firmware, an unknown play and an image too small to be a flash are each refused", async () => {
    const none = loaderFor(site(() => json(answer({ available: false }))).relay);
    await none.loader.play("12");
    expect(none.message()).toBe("play 12 has no firmware to download");
    const missing = loaderFor(site(() => json({ detail: "no such play" }, 404)).relay);
    await missing.loader.play("999999");
    expect(missing.message()).toBe("ai-passport.folotoy.cn has no play 999999");
    const small = mergedBin(0x1000);
    const tiny = loaderFor(site(async () => json(answer(await described(small))), () => binary(small)).relay);
    await tiny.loader.play("1039");
    expect(tiny.state()).toBe("refused");
    expect(tiny.message()).toBe("`play-1039-r2347.bin` is 4.0 KiB: a merged flash image holds the partition table at 0x8000");
    expect([...none.seen, ...missing.seen, ...tiny.seen]).toEqual([]);
  });

  test("a play still downloading when a firmware is dropped is not booted over it", async () => {
    const bytes = mergedBin();
    let release: (response: Response) => void = () => {};
    const held = new Promise<Response>((resolve) => {
      release = resolve;
    });
    const page = loaderFor(site(async () => json(answer(await described(bytes))), () => held).relay);
    const playing = page.loader.play("1039");
    while (page.loader.store.get().steps.length < 3) {
      await new Promise((resolve) => setTimeout(resolve, 0));
    }
    const dropped = mergedBin(0x10_000);
    await page.loader.offer({ root: null, files: [{ path: "mine.bin", size: dropped.length, read: () => Promise.resolve(dropped) }] });
    release(binary(bytes));
    await playing;
    expect(page.seen.map((image) => image.name)).toEqual(["mine"]);
    expect(page.loader.store.get().image).toBe("mine");
    expect(page.state()).toBe("loaded");
  });

  test("a page given no relay refuses a play as having none", async () => {
    const page = loaderFor(undefined);
    await page.loader.play("1039");
    expect(page.loader.store.get().message).toEqual({ kind: "play-fault", id: 1039, fault: { kind: "no-relay" } });
  });
});
