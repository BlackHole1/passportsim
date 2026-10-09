// The play box on the mounted page: what is typed reaches the relay on the page's own origin and
// nothing else, and the firmware it answers with is handed to the Worker as a boot.

import { describe, expect, test } from "bun:test";
import { IMAGE_MAGIC } from "./load";
import { createPage, type Page } from "./page";
import { PLAY_RELAY_HEADER, type PlayFetch } from "./play";
import { installDom, settle, settleUntil } from "./view/testDom";

const window = installDom();
const document = window.document as unknown as Document;

interface Mounted {
  readonly page: Page;
  readonly mount: HTMLElement;
  readonly toWorker: { type?: string; config?: string; assets?: { bytes: Uint8Array }[] }[];
}

function mountPage(fetchPlay: PlayFetch, search = "?mode=simple&lang=en"): Mounted {
  const mount = document.createElement("div");
  document.body.appendChild(mount);
  const toWorker: Mounted["toWorker"] = [];
  let clock = 0;
  const page = createPage({
    mount,
    transport: () => Promise.resolve({ ok: JSON.stringify({ json: {}, text: "" }) }),
    toWorker: (message) => toWorker.push(message as Mounted["toWorker"][number]),
    rewindSource: { snapshot: () => new Uint8Array(0), frame: () => null },
    now: () => (clock += 3),
    confirm: () => true,
    viewport: () => ({ width: 1440, height: 900 }),
    devicePixelRatio: () => 1,
    storage: () => null,
    search,
    systemLocale: () => "en-US",
    playRelay: { base: "https://passportsim.test/play-site/", fetch: fetchPlay },
  });
  return { page, mount, toWorker };
}

function firmware(): Uint8Array {
  const bytes = new Uint8Array(0x10_000);
  bytes[0] = IMAGE_MAGIC;
  return bytes;
}

async function playSite(bytes: Uint8Array): Promise<{ fetchPlay: PlayFetch; urls: string[] }> {
  const digest = await crypto.subtle.digest("SHA-256", bytes as Uint8Array<ArrayBuffer>);
  const sha256 = [...new Uint8Array(digest)].map((byte) => byte.toString(16).padStart(2, "0")).join("");
  const urls: string[] = [];
  const fetchPlay: PlayFetch = (url) => {
    urls.push(url);
    return Promise.resolve(
      url.includes("/api/plays/id/")
        ? new Response(
            JSON.stringify({
              ok: true,
              play: {
                id: 1039,
                revisionId: 2347,
                title: { zh: "口袋天气", en: "Pocket Weather" },
                firmware: { available: true, size: bytes.length, sha256, url: "/api/download/community/community-b6eb4756", format: "esp-merged-0x0" },
              },
            }),
            { headers: { [PLAY_RELAY_HEADER]: "1" } },
          )
        : new Response(bytes as Uint8Array<ArrayBuffer>, { headers: { [PLAY_RELAY_HEADER]: "1" } }),
    );
  };
  return { fetchPlay, urls };
}

function submit(mount: HTMLElement, text: string): void {
  const input = mount.querySelector("[data-play-input]") as HTMLInputElement;
  input.value = text;
  const form = mount.querySelector("[data-play-form]") as HTMLFormElement;
  form.dispatchEvent(new window.Event("submit", { bubbles: true, cancelable: true }) as unknown as Event);
}

const loaderState = (mount: HTMLElement) => mount.querySelector("[data-loader]")?.getAttribute("data-loader-state");
const boots = (mounted: Mounted) => mounted.toWorker.filter((message) => message.type === "boot");

for (const mode of ["simple", "advanced"] as const) {
  describe(`the play box in ${mode} mode`, () => {
    test("says which site the firmware comes from, in one form with one input", async () => {
      const { mount } = mountPage(() => Promise.reject(new Error("unused")), `?mode=${mode}&lang=en`);
      await settle();
      expect(mount.querySelectorAll("[data-play-form]").length).toBe(1);
      const form = mount.querySelector("[data-loader] [data-play-form]") as HTMLElement;
      expect(form.querySelector("label")?.textContent).toBe("Or load a play from ai-passport.folotoy.cn, by its link or number");
      expect((form.querySelector("[data-play-input]") as HTMLInputElement).placeholder).toBe("https://ai-passport.folotoy.cn/plays/22/");
      expect(form.textContent).toContain("The firmware comes from ai-passport.folotoy.cn through this site");
      expect(form.querySelector('[data-action="load-play"]')?.textContent).toBe("Load");
    });

    test("a play's link downloads its firmware through the relay and boots it", async () => {
      const bytes = firmware();
      const { fetchPlay, urls } = await playSite(bytes);
      const mounted = mountPage(fetchPlay, `?mode=${mode}&lang=en`);
      await settle();
      const before = boots(mounted).length;
      submit(mounted.mount, "https://ai-passport.folotoy.cn/plays/1039/");
      await settleUntil(() => boots(mounted).length > before);
      expect(urls).toEqual(["https://passportsim.test/play-site/api/plays/id/1039", "https://passportsim.test/play-site/api/download/community/community-b6eb4756"]);
      const boot = boots(mounted).at(-1);
      expect(JSON.parse(boot?.config ?? "{}")).toEqual({ fw: "play-1039-r2347" });
      expect(boot?.assets?.map((asset) => asset.bytes.length)).toEqual([bytes.length]);
      expect(loaderState(mounted.mount)).toBe("loading");
      expect(mounted.mount.querySelector("[data-play-page]")).toBeNull();
    });

    test("a server with no relay is said on the page, with a link to the play's own page", async () => {
      const mounted = mountPage(() => Promise.resolve(new Response("not found", { status: 404 })), `?mode=${mode}&lang=en`);
      await settle();
      const before = boots(mounted).length;
      submit(mounted.mount, "1039");
      await settleUntil(() => loaderState(mounted.mount) === "refused");
      expect(loaderState(mounted.mount)).toBe("refused");
      expect(mounted.mount.querySelector("[data-loader-message]")?.textContent).toContain("this server cannot load a play directly from ai-passport.folotoy.cn");
      const link = mounted.mount.querySelector("[data-play-page]") as HTMLAnchorElement;
      expect(link.getAttribute("href")).toBe("https://ai-passport.folotoy.cn/plays/1039/");
      expect(link.getAttribute("target")).toBe("_blank");
      expect(link.getAttribute("rel")).toBe("noreferrer");
      expect(link.textContent).toBe("Open play 1039 on ai-passport.folotoy.cn");
      expect(boots(mounted).length).toBe(before);
    });

    test("a link to another site asks nothing of any site", async () => {
      const urls: string[] = [];
      const mounted = mountPage((url) => {
        urls.push(url);
        return Promise.reject(new Error("unused"));
      }, `?mode=${mode}&lang=en`);
      await settle();
      submit(mounted.mount, "https://example.com/plays/1039/");
      await settleUntil(() => loaderState(mounted.mount) === "refused");
      expect(mounted.mount.querySelector("[data-loader-message]")?.textContent).toBe(
        "`example.com` is not ai-passport.folotoy.cn: the page loads plays from ai-passport.folotoy.cn only",
      );
      expect(mounted.mount.querySelector("[data-play-page]")).toBeNull();
      expect(urls).toEqual([]);
    });
  });
}

test("a Chinese reader is shown the play's Chinese title", async () => {
  const { fetchPlay } = await playSite(firmware());
  const mounted = mountPage(fetchPlay, "?mode=simple&lang=zh-CN");
  await settle();
  submit(mounted.mount, "1039");
  await settleUntil(() => (mounted.mount.querySelector(".log-panel")?.textContent ?? "").includes("口袋天气"));
  expect(mounted.mount.querySelector(".log-panel")?.textContent).toContain("玩法 1039，口袋天气：修订 2347");
  expect(mounted.mount.querySelector("[data-play-form] label")?.textContent).toBe("或从 ai-passport.folotoy.cn 加载玩法，填链接或编号");
});
