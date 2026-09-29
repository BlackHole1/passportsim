// The Wi-Fi HTTP probe in the browser, through the helper relay. A daemon
// (`crates/pemu-host/examples/attach_daemon.rs`) serves the page, which redeems the launch code and
// boots `official`, learning the daemon's port. Each test then loads `probe_wifi_http` through the
// loader, pauses, and puts the probe's open access point on the air.
//
// | Test | The GET is answered by | What it shows |
// |---|---|---|
// | no host | the virtual LAN's scripted service, inside the page | association, the DHCP lease of `10.23.0.100`, the 200 and the body hash, with no host anywhere |
// | relay | an HTTP server this file runs on `127.0.0.1`, over `ws://127.0.0.1:<port>/v1/relay` | the same run with the answering side moved out of the page, carried by `relay.ts` and `relay_wisp.rs` |
//
// DHCP never leaves the machine in either test, so the carried GET is the one thing the relay does.
// The relay test's body is this file's, not the LAN's: `net_http --op status` reporting
// `service.answers` 0 with the host's hash on the console is what tells a carried GET apart.

import { spawn, spawnSync, type ChildProcessWithoutNullStreams } from "node:child_process";
import { createHash } from "node:crypto";
import { createServer, type Server } from "node:http";
import { existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, isAbsolute, join } from "node:path";
import { fileURLToPath } from "node:url";
import { createInterface } from "node:readline";
import { expect, test, type Page } from "@playwright/test";
import { browserGaps, call, consoleText, loadImage, pinPrefs, waitForLine, waitForStatus } from "./harness";
import { pebundle, readOfficialFiles, type DemoFile } from "./demoBundle";
import { findCore } from "./preconditions";

const WEB = join(dirname(fileURLToPath(import.meta.url)), "..");
const REPO = join(WEB, "..");

/** The variable `xtask ci` names the daemon it built with (`xtask/src/ci/tiers.rs`). */
const DAEMON_VARIABLE = "PEMU_E2E_ATTACH_DAEMON";

/** The open SSID `probe_wifi_http` joins (its `Kconfig.projbuild` placeholder default). */
const VIRTUAL_AP = "passport-emu-virtual-ap";

/** The guest port the probe's `esp_http_client` reaches (`pemu_radio::lan::services::HTTP_PORT`). */
const GUEST_HTTP_PORT = 80;

/** The body the host server answers with: not the virtual LAN's, so the two cannot be confused. */
const HOST_BODY = "passportsim host server, carried by the browser relay\n";

/**
 * The virtual LAN's own body (`pemu_radio::lan::services::PROBE_BODY`). The page reports its length
 * as `service.body_bytes`, so a disagreement with the core fails on that field too.
 */
const LAN_BODY = "passportsim virtual LAN: scripted HTTP service\n";

const PAGE_BOOT_MS = 90_000;

const PROBE_MS = 120_000;

for (const { applies, gap } of browserGaps()) {
  test.skip(({ browserName, channel }) => applies(browserName, channel), gap);
}

interface Daemon {
  readonly child: ChildProcessWithoutNullStreams;
  readonly port: number;
  readonly token: string;
  readonly launch: string;
  readonly stderr: string[];
}

/** The daemon binary: the one `xtask ci` built, or a debug build made here (as `attach.spec.ts`). */
function daemonBinary(): string {
  const given = process.env[DAEMON_VARIABLE];
  if (given !== undefined && given !== "") {
    if (!existsSync(given)) {
      throw new Error(`${DAEMON_VARIABLE} names ${given}, which does not exist: the run asked for that daemon`);
    }
    return given;
  }
  const built = spawnSync("cargo", ["build", "-p", "pemu-host", "--example", "attach_daemon"], {
    cwd: REPO,
    encoding: "utf8",
  });
  if (built.status !== 0) {
    throw new Error(`the attach daemon did not build:\n${built.stderr.slice(-4000)}`);
  }
  const target = process.env.CARGO_TARGET_DIR
    ? isAbsolute(process.env.CARGO_TARGET_DIR)
      ? process.env.CARGO_TARGET_DIR
      : join(REPO, process.env.CARGO_TARGET_DIR)
    : join(REPO, "target");
  return join(target, "debug", "examples", process.platform === "win32" ? "attach_daemon.exe" : "attach_daemon");
}

async function startDaemon(core: string, bundle: string): Promise<Daemon> {
  const child = spawn(daemonBinary(), ["--web", join(WEB, "dist"), "--core", core, "--bundle", `official=${bundle}`], {
    stdio: ["pipe", "pipe", "pipe"],
  });
  const stderr: string[] = [];
  child.stderr.setEncoding("utf8");
  child.stderr.on("data", (chunk: string) => stderr.push(chunk));
  const lines = createInterface({ input: child.stdout });
  const first = await new Promise<string>((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error(`the daemon printed nothing in 30 s: ${stderr.join("")}`)), 30_000);
    lines.once("line", (line) => {
      clearTimeout(timer);
      resolve(line);
    });
    child.once("exit", (code) => {
      clearTimeout(timer);
      reject(new Error(`the daemon exited (${code}) before printing its port: ${stderr.join("")}`));
    });
  });
  const printed = JSON.parse(first) as { port: number; token: string; launch: string };
  return { child, stderr, ...printed };
}

/** Stops a daemon: closing its standard input is its stop signal (`attach_daemon.rs`). */
async function stopDaemon(daemon: Daemon): Promise<void> {
  const exited = new Promise<void>((resolve) => daemon.child.once("exit", () => resolve()));
  daemon.child.stdin.end();
  const timer = setTimeout(() => daemon.child.kill("SIGKILL"), 10_000);
  await exited;
  clearTimeout(timer);
}

/** The host server the bridge reaches: answers `GET /probe` with {@link HOST_BODY} and nothing else. */
interface HostServer {
  readonly server: Server;
  readonly port: number;
  readonly paths: string[];
}

async function startHostServer(): Promise<HostServer> {
  const paths: string[] = [];
  const server = createServer((request, response) => {
    paths.push(request.url ?? "");
    if (request.url === "/probe") {
      response.writeHead(200, { "content-type": "text/plain", "content-length": Buffer.byteLength(HOST_BODY) });
      response.end(HOST_BODY);
      return;
    }
    response.writeHead(404, { "content-length": 0 });
    response.end();
  });
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  const address = server.address();
  if (address === null || typeof address === "string") {
    throw new Error("the host server did not bind a loopback port");
  }
  return { server, port: address.port, paths };
}

/**
 * The `probe_wifi_http` build from the corpus `probes` directory under `PASSPORTSIM_DATA_ROOT`,
 * checked against `tests/fw/manifest.toml` as `tests/milestones/m12.rs` `probe_wifi_http_files`
 * does. The ELF is the unstripped one: the committed copy carries no symbol a hook could bind by.
 * A file that is not the pinned build fails; an absent one skips.
 */
function readProbeFiles(
  env: Readonly<Record<string, string | undefined>> = process.env,
): { files: DemoFile[] } | { skip: string } | { mismatch: string } {
  const root = env.PASSPORTSIM_DATA_ROOT;
  if (root === undefined || root === "") {
    return { skip: "no data root: set PASSPORTSIM_DATA_ROOT ; probe builds are not committed" };
  }
  const probes = join(root, "corpus", "probes");
  const wanted = [
    { role: "flash", name: "probe_wifi_http-8MB.bin", path: join(probes, "probe_wifi_http-8MB.bin"), pin: "merged_sha256" },
    {
      role: "app_elf",
      name: "probe_wifi_http.elf",
      path: join(probes, "build", "probe_wifi_http", "probe_wifi_http.elf"),
      pin: "elf_sha256",
    },
  ] as const;
  const manifest = readFileSync(join(REPO, "tests", "fw", "manifest.toml"), "utf8");
  const block = manifest.split("[[probe]]").find((part) => part.includes('name = "probe_wifi_http"'));
  if (block === undefined) {
    throw new Error("tests/fw/manifest.toml pins no probe_wifi_http");
  }
  const files: DemoFile[] = [];
  for (const file of wanted) {
    if (!existsSync(file.path)) {
      return { skip: `no \`probe_wifi_http\` ${file.role} at ${file.path}: the probe is not built (\`cargo xtask probes\`)` };
    }
    const bytes = readFileSync(file.path);
    const sha256 = createHash("sha256").update(bytes).digest("hex");
    const pinned = block.split("\n").find((line) => line.startsWith(`${file.pin} = "`));
    const want = pinned?.slice(`${file.pin} = "`.length).replace(/"$/, "");
    if (want === undefined) {
      throw new Error(`tests/fw/manifest.toml pins no ${file.pin} for probe_wifi_http`);
    }
    if (sha256 !== want) {
      return { mismatch: `the ${file.role} at ${file.path} is not the pinned probe build (SHA-256 ${sha256})` };
    }
    files.push({ role: file.role, name: file.name, bytes, sha256 });
  }
  return { files };
}

async function lanStatus(page: Page): Promise<Record<string, unknown>> {
  const answer = await call(page, "net_http", {});
  expect(answer.ok, `net_http status in the page: ${answer.ok ? "" : answer.error}`).toBe(true);
  const json = (answer.ok ? answer.json : {}) as { lan?: Record<string, unknown> };
  return json.lan ?? {};
}

function preconditions():
  | { kind: "skip"; reason: string }
  | { kind: "run"; core: string; official: DemoFile[]; probe: DemoFile[] } {
  const core = findCore(process.env);
  if ("skip" in core) {
    return { kind: "skip", reason: core.skip };
  }
  const official = readOfficialFiles();
  if ("mismatch" in official) {
    throw new Error(official.mismatch);
  }
  if ("skip" in official) {
    return { kind: "skip", reason: official.skip };
  }
  const probe = readProbeFiles();
  if ("mismatch" in probe) {
    throw new Error(probe.mismatch);
  }
  if ("skip" in probe) {
    return { kind: "skip", reason: probe.skip };
  }
  return { kind: "run", core: core.path, official: official.files, probe: probe.files };
}

/**
 * The two `.pebundle` files a test drops: the demo the page boots from the daemon, and the probe
 * (flash image and unstripped ELF) that replaces it.
 */
function bundles(pre: { official: DemoFile[]; probe: DemoFile[] }, scratch: string): { demo: string; probe: string } {
  const demo = join(scratch, "official.pebundle");
  writeFileSync(demo, pebundle(pre.official));
  const probe = join(scratch, "probe_wifi_http.pebundle");
  writeFileSync(probe, pebundle(pre.probe, { id: "probe_wifi_http", name: "probe_wifi_http" }));
  return { demo, probe };
}

/**
 * Opens the daemon-served page, loads the probe over the demo, pauses it and puts its access point
 * on the air. The pause avoids a race: the page resumes a load at `ready`, and the probe calls
 * `esp_wifi_connect` a few hundred virtual milliseconds after start.
 */
async function openProbePage(page: Page, daemon: Daemon, probeBundle: string, row: string): Promise<void> {
  // The launch URL; its attach tells the Worker the daemon's port, where the relay lives too.
  await pinPrefs(page);
  await page.goto(daemon.launch);
  const mount = page.locator("#app");
  await expect(mount, "the page attached to the daemon").toHaveAttribute("data-attached", /^b\d+$/, {
    timeout: PAGE_BOOT_MS,
  });
  console.log(`RAN ${row} attach: the daemon minted \`${await mount.getAttribute("data-attached")}\` for the page`);

  await loadImage(page, {
    image: "probe_wifi_http",
    variable: "PASSPORTSIM_DATA_ROOT",
    root: probeBundle,
    directory: false,
    name: "probe_wifi_http",
    files: [{ relative: "probe_wifi_http.pebundle", path: probeBundle }],
  });
  await page.locator('[data-action="pause"]').click();
  const status = await waitForStatus(page);
  expect(status, "`status` answers once the probe is running").toMatchObject({ ok: true });
  expect(JSON.stringify(status.ok ? status.json : null), "the page runs the probe").toContain(
    '"fw":"probe_wifi_http"',
  );
  // The open access point the probe's placeholder SSID names.
  const ap = await call(page, "wifi_ap", { ssid: VIRTUAL_AP, channel: 6, rssi: -40 });
  expect(ap.ok, `wifi_ap: ${ap.ok ? "" : ap.error}`).toBe(true);
}

async function runProbe(page: Page): Promise<string> {
  await page.locator('[data-action="run"]').click();
  await waitForLine(page, /DONE\|name=probe_wifi_http/, PROBE_MS);
  const text = await consoleText(page);
  expect(text, "no step of the probe failed").not.toContain("FAIL|");
  expect(text, "the probe leased an address").toMatch(/WIFI\|[^\n]*\|leased=1\|/);
  expect(text, "from the virtual LAN's gateway").toContain(
    "IP|ip=10.23.0.100|netmask=255.255.255.0|gw=10.23.0.1",
  );
  expect(text, "the probe stopped and deinited cleanly").toContain("RC|stop=0|deinit=0");
  expect(text, "and said so").toContain("DONE|name=probe_wifi_http|status=ok");
  return text;
}

/**
 * The probe in the browser with no host anywhere: it associates, lwIP leases an address from the
 * virtual LAN, and the LAN's own scripted service answers the GET. The relay test below replaces
 * exactly the side that answers.
 */
test("probe_wifi_http runs in the browser with no host at all @chromium-only", async ({
  page,
}) => {
  test.setTimeout(600_000);
  const pre = preconditions();
  test.skip(pre.kind === "skip", pre.kind === "skip" ? pre.reason : "");
  if (pre.kind !== "run") {
    return;
  }
  const scratch = mkdtempSync(join(tmpdir(), "pemu-wifi-http-web-"));
  const files = bundles(pre, scratch);
  const daemon = await startDaemon(pre.core, files.demo);
  try {
    await openProbePage(page, daemon, files.probe, "no-host");
    const text = await runProbe(page);
    expect(text, "the scripted service answered the GET with its own body").toContain(
      `HTTP|path=/probe|rc=0|status=200|length=${LAN_BODY.length}|sha256=${createHash("sha256").update(LAN_BODY).digest("hex")}`,
    );
    const lan = await lanStatus(page);
    const service = lan.service as { answers?: number; body_bytes?: number } | undefined;
    const bridge = lan.bridge as { attached?: boolean } | undefined;
    expect(service?.body_bytes, "the body is the one this test hashed").toBe(LAN_BODY.length);
    expect(service?.answers, "the virtual LAN answered the GET itself").toBe(1);
    expect(bridge?.attached, "with no bridge anywhere").toBe(false);
    console.log(
      `RAN no-host: leased 10.23.0.100 and the virtual LAN answered ${String(service?.answers)} GET`,
    );
  } finally {
    await stopDaemon(daemon);
    rmSync(scratch, { recursive: true, force: true });
  }
});

test("probe_wifi_http leases an address in the browser and its GET is answered over the helper relay @chromium-only", async ({
  page,
}) => {
  test.setTimeout(600_000);
  const pre = preconditions();
  test.skip(pre.kind === "skip", pre.kind === "skip" ? pre.reason : "");
  if (pre.kind !== "run") {
    return;
  }
  const scratch = mkdtempSync(join(tmpdir(), "pemu-wifi-http-"));
  const files = bundles(pre, scratch);
  const host = await startHostServer();
  const daemon = await startDaemon(pre.core, files.demo);
  try {
    await openProbePage(page, daemon, files.probe, "relay");

    // The bridge: the guest's port 80 reaches this file's server and nothing else.
    const bridged = await call(page, "net_http", { op: "bridge", port: GUEST_HTTP_PORT, host_port: host.port });
    expect(bridged.ok, `net_http bridge: ${bridged.ok ? "" : bridged.error}`).toBe(true);
    expect(JSON.stringify(bridged.ok ? bridged.json : null), "the page's build leaves the packets to another carrier").toContain(
      "another carrier moves its packets",
    );
    console.log(`RAN relay bridge: ${VIRTUAL_AP} is on the air and guest port 80 reaches 127.0.0.1:${host.port}`);

    // DHCP from the virtual LAN; the GET leaves the page over the relay.
    const console_ = await runProbe(page);
    const hash = createHash("sha256").update(HOST_BODY).digest("hex");
    expect(console_, "the GET was answered by the host server, hashed by the guest as it arrived").toContain(
      `HTTP|path=/probe|rc=0|status=200|length=${Buffer.byteLength(HOST_BODY)}|sha256=${hash}`,
    );

    // The host server saw the request and the virtual LAN answered nothing, so the 200 is not its.
    expect(host.paths, "the request reached the host server through the relay").toEqual(["/probe"]);
    const lan = await lanStatus(page);
    const service = lan.service as { answers?: number } | undefined;
    const bridge = lan.bridge as Record<string, unknown> | undefined;
    expect(service?.answers, "the virtual LAN's scripted service answered nothing").toBe(0);
    expect(bridge?.attached, "the bridge is still live").toBe(true);
    expect(bridge?.streams, "one stream was opened for the GET").toBe(1);
    expect(bridge?.refused, "and none was refused").toBe(0);
    expect(Number(bridge?.bytes_out ?? 0), "the request left the page").toBeGreaterThan(0);
    expect(Number(bridge?.bytes_in ?? 0), "and the answer came back").toBeGreaterThan(0);
    expect(Number(bridge?.lost_in ?? 0), "with no packet lost on the way in").toBe(0);
    expect(Number(bridge?.dropped_out ?? 0), "and none dropped on the way out").toBe(0);
    console.log(
      `RAN relay: ${String(bridge?.bytes_out)} B out and ${String(bridge?.bytes_in)} B in over ` +
        `ws://127.0.0.1:${daemon.port}/v1/relay; the virtual LAN answered ${String(service?.answers)} HTTP requests`,
    );

    // The run is `live`, not deterministic, because a live host peer answered into it
    // (`Origin::Bridge`): every server packet was journaled as a bridged `NetFrame`. The class is on
    // the receipt, which `harness.call` does not carry, so this call goes through `window.passportEmu`.
    const determinism = await page.evaluate(async () => {
      const api = (globalThis as unknown as {
        passportEmu: { call(n: string, a: unknown): Promise<{ receipt?: { determinism?: string } }> };
      }).passportEmu;
      return (await api.call("status", {})).receipt?.determinism ?? null;
    });
    expect(determinism, "a bridged run says what it is").toBe("live");
  } finally {
    await stopDaemon(daemon);
    await new Promise<void>((resolve) => host.server.close(() => resolve()));
    rmSync(scratch, { recursive: true, force: true });
  }
});
