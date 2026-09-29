// An MCP client drives a browser-hosted instance `b1` through the daemon attach path.
//
// A daemon (`crates/pemu-host/examples/attach_daemon.rs`, serving `web/dist`) serves the page; the
// page redeems the launch code, boots `official` in its Worker and attaches over
// `ws://127.0.0.1:<port>/v1/attach`, where the daemon gives it a `b` id. This test is then the MCP
// client: JSON-RPC `tools/call` over `/mcp` with the bearer token, addressing that id.
//
// The daemon has no native machine, so `/v1/instances` lists only the page's id. Each leg is then
// compared with what the page's own in-page registry says about its machine:
//
// | Leg | Through the daemon | Against the page |
// |---|---|---|
// | status | `passport_status` names the `b` id and `official`, at a virtual time | the page's `status` right after is at or past that time, on the same image |
// | serial read | `passport_serial` returns the console the page's machine printed | it holds `official`'s menu line, which only a machine that booted it can print |
// | input | `passport_input` clicks DOWN on the `b` id | the page's own `inspect vars` reads `s_sel` 0 before and 1 after |
// | detach | the page closes | the next call on the id is refused `E_STATE`, and not answered natively |

import { spawn, spawnSync, type ChildProcessWithoutNullStreams } from "node:child_process";
import { existsSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, isAbsolute, join } from "node:path";
import { fileURLToPath } from "node:url";
import { createInterface } from "node:readline";
import { expect, test, type Page } from "@playwright/test";
import { browserGaps, call, pinPrefs } from "./harness";
import { pebundle, readOfficialFiles } from "./demoBundle";
import { findCore } from "./preconditions";

const WEB = join(dirname(fileURLToPath(import.meta.url)), "..");
const REPO = join(WEB, "..");

/** The variable `xtask ci` names the daemon it built with (`xtask/src/ci/tiers.rs`). */
const DAEMON_VARIABLE = "PEMU_E2E_ATTACH_DAEMON";

/** The `official` console line the settled menu follows (`main.c`). */
const OFFICIAL_READY = "main: 就绪";

const PAGE_BOOT_MS = 90_000;

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

/**
 * The daemon binary: the one `xtask ci` built ({@link DAEMON_VARIABLE}), or a debug build made here.
 * A variable naming a missing file throws.
 */
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
  const child = spawn(
    daemonBinary(),
    ["--web", join(WEB, "dist"), "--core", core, "--bundle", `official=${bundle}`],
    { stdio: ["pipe", "pipe", "pipe"] },
  );
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

interface ToolAnswer {
  readonly isError: boolean;
  readonly structured: Record<string, unknown>;
  readonly text: string;
}

let rpcId = 0;

/** One JSON-RPC request to `/mcp`, as an MCP client over streamable HTTP sends it. */
async function mcp(daemon: Daemon, method: string, params: unknown): Promise<Record<string, unknown>> {
  rpcId += 1;
  const response = await fetch(`http://127.0.0.1:${daemon.port}/mcp`, {
    method: "POST",
    headers: {
      authorization: `Bearer ${daemon.token}`,
      "content-type": "application/json",
      accept: "application/json, text/event-stream",
    },
    body: JSON.stringify({ jsonrpc: "2.0", id: rpcId, method, params }),
  });
  const body = (await response.json()) as Record<string, unknown>;
  expect(response.status, `\`${method}\` over /mcp: ${JSON.stringify(body)}`).toBe(200);
  return body;
}

async function tool(daemon: Daemon, name: string, args: Record<string, unknown>): Promise<ToolAnswer> {
  const body = await mcp(daemon, "tools/call", { name, arguments: args });
  const result = body.result as
    | { isError?: boolean; structuredContent?: Record<string, unknown>; content?: { text?: string }[] }
    | undefined;
  expect(result, `\`${name}\` has a result: ${JSON.stringify(body)}`).toBeDefined();
  return {
    isError: result?.isError === true,
    structured: result?.structuredContent ?? {},
    text: (result?.content ?? []).map((c) => c.text ?? "").join("\n"),
  };
}

function firstInstance(status: Record<string, unknown>): Record<string, unknown> {
  const rows = status.instances as Record<string, unknown>[] | undefined;
  expect(rows, `status lists one machine: ${JSON.stringify(status).slice(0, 400)}`).toHaveLength(1);
  return rows?.[0] ?? {};
}

async function instances(daemon: Daemon): Promise<string[]> {
  const response = await fetch(`http://127.0.0.1:${daemon.port}/v1/instances`, {
    headers: { authorization: `Bearer ${daemon.token}` },
  });
  const body = (await response.json()) as { result?: { instances?: string[] } };
  return body.result?.instances ?? [];
}

/** The page's own `s_sel`, through its in-page registry (`m9.spec.ts` reads it the same way). */
async function pageSSel(page: Page): Promise<unknown> {
  const answer = await call(page, "inspect", { what: ["vars"], vars: ["s_sel"] });
  expect(answer.ok, `the page's own inspect vars: ${answer.ok ? "" : answer.error}`).toBe(true);
  const rows = (answer.ok ? answer.json : {}) as { vars?: { name?: string; value?: unknown; unreadable?: string }[] };
  expect(rows.vars?.[0]?.unreadable, "s_sel is readable in the page").toBeUndefined();
  return rows.vars?.[0]?.value;
}

async function poll<T>(read: () => Promise<T>, done: (value: T) => boolean, timeoutMs: number): Promise<T> {
  const deadline = Date.now() + timeoutMs;
  let value = await read();
  while (!done(value) && Date.now() < deadline) {
    await new Promise((resolve) => setTimeout(resolve, 250));
    value = await read();
  }
  return value;
}

test("an MCP client drives the browser-hosted instance b1 through the daemon attach path @chromium-only", async ({
  page,
}) => {
  test.setTimeout(600_000);
  const core = findCore(process.env);
  test.skip("skip" in core, "skip" in core ? core.skip : "");
  const official = readOfficialFiles();
  if ("mismatch" in official) {
    throw new Error(official.mismatch);
  }
  test.skip("skip" in official, "skip" in official ? official.skip : "");
  if (!("path" in core) || !("files" in official)) {
    return;
  }
  const scratch = mkdtempSync(join(tmpdir(), "pemu-attach-"));
  const bundle = join(scratch, "official.pebundle");
  writeFileSync(bundle, pebundle(official.files));
  const daemon = await startDaemon(core.path, bundle);
  try {
    // The launch URL, whose shell redeems the code and loads the UI.
    await pinPrefs(page);
    await page.goto(daemon.launch);
    const mount = page.locator("#app");
    await expect(mount, "the page attached to the daemon").toHaveAttribute("data-attached", /^b\d+$/, {
      timeout: PAGE_BOOT_MS,
    });
    const id = (await mount.getAttribute("data-attached")) ?? "";
    console.log(`RAN attach: the daemon minted \`${id}\` for the page`);

    // No native machine: the page's id is the only one the daemon routes.
    expect(await instances(daemon), "the daemon's instances are the page alone").toEqual([id]);

    // Status through MCP, then through the page: one machine, on `official`, its clock moving on.
    const viaMcp = await tool(daemon, "passport_status", { instance: id });
    expect(viaMcp.isError, `passport_status on ${id}: ${viaMcp.text}`).toBe(false);
    const mcpRow = firstInstance(viaMcp.structured);
    expect(mcpRow.instance, "the status names the daemon's id, not the page's `p1`").toBe(id);
    const inPage = await call(page, "status", {});
    expect(inPage.ok, `the page's own status: ${inPage.ok ? "" : inPage.error}`).toBe(true);
    const pageStatus = (inPage.ok ? inPage.json : {}) as Record<string, unknown>;
    const pageRow = firstInstance(pageStatus);
    expect(mcpRow.fw, "the status names `official`").toBe("official");
    expect(pageRow.fw, "the page runs `official` too").toBe("official");
    expect(pageRow.instance, "the page names its own machine by the wasm pool's id").not.toBe(id);
    const mcpVt = Number(mcpRow.vt_us);
    const pageVt = Number(pageRow.vt_us);
    expect(Number.isFinite(mcpVt) && mcpVt > 0, `a virtual time in ${JSON.stringify(mcpRow)}`).toBe(true);
    expect(pageVt, "the page's clock is at or past the time MCP was told").toBeGreaterThanOrEqual(mcpVt);
    console.log(`RAN status: ${id} at ${mcpVt} us through MCP, the page at ${pageVt} us after it`);

    const serial = await poll(
      () => tool(daemon, "passport_serial", { instance: id, op: "read" }),
      (answer) => answer.isError || JSON.stringify(answer.structured).includes(OFFICIAL_READY),
      PAGE_BOOT_MS,
    );
    expect(serial.isError, `passport_serial on ${id}: ${serial.text}`).toBe(false);
    expect(JSON.stringify(serial.structured), "the page's console reached the menu").toContain(OFFICIAL_READY);
    console.log(`RAN serial: ${id}'s console holds \`${OFFICIAL_READY}\``);

    // Input through MCP, observed by the page's registry reading guest memory.
    expect(await pageSSel(page), "s_sel before the click").toBe(0);
    const click = await tool(daemon, "passport_input", { instance: id, button: "down", action: "click" });
    expect(click.isError, `passport_input on ${id}: ${click.text}`).toBe(false);
    const after = await poll(
      () => pageSSel(page),
      (value) => value === 1,
      15_000,
    );
    expect(after, "the page's own s_sel after the MCP click DOWN").toBe(1);
    console.log(`RAN input: the MCP click DOWN on ${id} moved the page's s_sel from 0 to 1`);

    // Detach: the id is refused rather than answered by anything else.
    await page.close();
    const gone = await poll(
      () => tool(daemon, "passport_status", { instance: id }),
      (answer) => answer.isError,
      15_000,
    );
    expect(gone.isError, `status on ${id} after the page closed: ${gone.text}`).toBe(true);
    const refusal = gone.structured.error as { code?: string } | undefined;
    expect(refusal?.code, `the refusal is E_STATE: ${JSON.stringify(gone.structured)}`).toBe("E_STATE");
    console.log(`RAN detach: ${id} is refused after its page closed: ${gone.text.split("\n")[0]}`);
  } finally {
    await stopDaemon(daemon);
    rmSync(scratch, { recursive: true, force: true });
  }
});
