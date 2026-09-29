// The host CPU a browser spends, read from outside it. A page cannot read its own process CPU time,
// so this reads the cumulative user plus system time of every browser process from `ps` on macOS
// and `Win32_Process` on Windows; a share of a core is CPU seconds gained over a wall interval.
// CPU time accrues only on a core, so it can be read on a busy host where wall budgets cannot.
//
// Browser processes: for Chromium, the test worker's descendants; for WebKit on macOS, the XPC
// services (parented by launchd) found by path inside the Playwright WebKit install. Another
// browser of that install makes the reading `shared`, which the caller must not gate on.
//
// Windows: the table is JSON from `Get-CimInstance Win32_Process` so no localized output is parsed.
// The docs say milliseconds, but the times count 100 ns units. PowerShell takes about a second to
// start and reads at its end, so a caller times from the end of one reading to the end of the next
// (`benchBrowser.spec.ts` `cpuOver`). `hostLoad` gives the load average on macOS and busy logical
// processors on Windows (`loadKind`).

import { spawnSync } from "node:child_process";
import { cpus, loadavg } from "node:os";

export interface Proc {
  readonly pid: number;
  readonly ppid: number;
  readonly cpuS: number;
  readonly command: string;
}

/** `[[dd-]hh:]mm:ss.cc` as `ps` prints `time`, in seconds. */
export function parseCpuTime(text: string): number {
  let rest = text.trim();
  let days = 0;
  const dash = rest.indexOf("-");
  if (dash > 0) {
    days = Number(rest.slice(0, dash));
    rest = rest.slice(dash + 1);
  }
  const parts = rest.split(":").map(Number);
  let seconds = 0;
  for (const part of parts) {
    seconds = seconds * 60 + part;
  }
  const total = days * 86_400 + seconds;
  if (!Number.isFinite(total)) {
    throw new Error(`\`ps\` printed a CPU time that does not parse: ${JSON.stringify(text)}`);
  }
  return total;
}

export function parseTable(text: string): Proc[] {
  const out: Proc[] = [];
  for (const line of text.split("\n")) {
    const found = /^\s*(\d+)\s+(\d+)\s+(\S+)\s+(.*)$/.exec(line);
    if (found === null) {
      continue;
    }
    out.push({
      pid: Number(found[1]),
      ppid: Number(found[2]),
      cpuS: parseCpuTime(found[3] ?? ""),
      command: found[4] ?? "",
    });
  }
  return out;
}

/** Parses the `Win32_Process` JSON; a process whose command line is unreadable still counts. */
export function parseWindowsTable(text: string): Proc[] {
  const parsed: unknown = JSON.parse(text);
  const rows = Array.isArray(parsed) ? parsed : [parsed];
  return rows.map((row) => {
    const r = row as Record<string, unknown>;
    const ticks = Number(r.KernelModeTime ?? 0) + Number(r.UserModeTime ?? 0);
    const pid = Number(r.ProcessId);
    const ppid = Number(r.ParentProcessId);
    if (!Number.isInteger(pid) || !Number.isInteger(ppid) || !Number.isFinite(ticks)) {
      throw new Error(`Win32_Process gave a row that does not parse: ${JSON.stringify(row).slice(0, 200)}`);
    }
    return { pid, ppid, cpuS: ticks / 1e7, command: typeof r.CommandLine === "string" ? r.CommandLine : "" };
  });
}

const WINDOWS_QUERY =
  "[Console]::OutputEncoding = [Text.Encoding]::UTF8; " +
  "Get-CimInstance Win32_Process | Select-Object ProcessId, ParentProcessId, KernelModeTime, UserModeTime, CommandLine | ConvertTo-Json -Compress";

/**
 * The process table now, without its reader: on Windows the PowerShell process and what it started
 * are descendants of this process, not the browser.
 */
export function processTable(platform: NodeJS.Platform = process.platform): Proc[] {
  if (platform === "win32") {
    const shell = spawnSync("powershell.exe", ["-NoProfile", "-NonInteractive", "-Command", WINDOWS_QUERY], {
      encoding: "utf8",
      maxBuffer: 64 * 1024 * 1024,
      windowsHide: true,
    });
    if (shell.status !== 0) {
      throw new Error(`reading Win32_Process failed (${shell.status}): ${shell.stderr}`);
    }
    const table = parseWindowsTable(shell.stdout);
    const reader = shell.pid ?? -1;
    const drop = new Set([reader, ...descendants(table, reader)]);
    return table.filter((p) => !drop.has(p.pid));
  }
  const ps = spawnSync("ps", ["-axo", "pid=,ppid=,time=,command="], { encoding: "utf8" });
  if (ps.status !== 0) {
    throw new Error(`\`ps\` failed: ${ps.stderr}`);
  }
  return parseTable(ps.stdout).filter((p) => p.pid !== ps.pid);
}

export function descendants(table: readonly Proc[], root: number): Set<number> {
  const children = new Map<number, number[]>();
  for (const p of table) {
    children.set(p.ppid, [...(children.get(p.ppid) ?? []), p.pid]);
  }
  const out = new Set<number>();
  const queue = [...(children.get(root) ?? [])];
  while (queue.length > 0) {
    const pid = queue.pop() as number;
    if (out.has(pid)) continue;
    out.add(pid);
    queue.push(...(children.get(pid) ?? []));
  }
  return out;
}

export interface BrowserProcs {
  readonly pids: Set<number>;
  readonly shared: string | null;
}

/**
 * The processes of the browser this test worker (`root`) launched. For WebKit, `installDir` is the
 * Playwright WebKit directory whose XPC services are counted.
 */
export function browserProcesses(
  table: readonly Proc[],
  root: number,
  engine: "chromium" | "webkit",
  installDir: string,
): BrowserProcs {
  const tree = descendants(table, root);
  const pids = new Set<number>(tree);
  let shared: string | null = null;
  if (engine === "webkit") {
    const inside = table.filter((p) => p.command.startsWith(installDir));
    for (const p of inside) {
      pids.add(p.pid);
    }
    // A browser of this install that is not ours: its XPC services are indistinguishable from ours.
    const foreign = inside.filter((p) => !tree.has(p.pid) && p.ppid !== 1);
    if (foreign.length > 0) {
      shared = `another process of ${installDir} is running outside this run (pid ${foreign.map((p) => p.pid).join(", ")}), so its XPC services cannot be told from this browser's`;
    }
  }
  return { pids, shared };
}

export function cpuOf(table: readonly Proc[], pids: ReadonlySet<number>): number {
  return table.filter((p) => pids.has(p.pid)).reduce((sum, p) => sum + p.cpuS, 0);
}

/**
 * PowerShell that switches execution-speed throttling (EcoQoS) off for the given process ids, one
 * `<pid> <error>` line each (0 for success). `SetProcessInformation` class 4, version 1,
 * `ControlMask` 1 (`EXECUTION_SPEED`), `StateMask` 0, on a `PROCESS_SET_INFORMATION` (0x0200) handle.
 */
const HIGH_QOS_SCRIPT = `
Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
public static class PemuHighQos {
  [StructLayout(LayoutKind.Sequential)]
  public struct State { public uint Version; public uint ControlMask; public uint StateMask; }
  [DllImport("kernel32.dll", SetLastError = true)]
  static extern IntPtr OpenProcess(uint access, bool inherit, uint pid);
  [DllImport("kernel32.dll", SetLastError = true)]
  static extern bool SetProcessInformation(IntPtr process, int infoClass, ref State info, uint size);
  [DllImport("kernel32.dll")]
  static extern bool CloseHandle(IntPtr handle);
  public static int Off(uint pid) {
    IntPtr h = OpenProcess(0x0200, false, pid);
    if (h == IntPtr.Zero) return Marshal.GetLastWin32Error();
    State s = new State { Version = 1, ControlMask = 1, StateMask = 0 };
    bool ok = SetProcessInformation(h, 4, ref s, (uint)Marshal.SizeOf(s));
    int error = ok ? 0 : Marshal.GetLastWin32Error();
    CloseHandle(h);
    return error;
  }
}
"@
foreach ($id in $PIDS) { "$id $([PemuHighQos]::Off([uint32]$id))" }
`;

export interface HighQos {
  readonly processes: number;
  readonly failed: readonly { readonly pid: number; readonly error: number }[];
}

/**
 * Asks Windows not to throttle the execution speed of `pids`; `null` on any other host. Windows 11
 * applies EcoQoS to a browser that has no visible window, which cut the interpreter to a fraction
 * of its steady speed. A browser a person is looking at is not throttled, so the figures are taken
 * with it off, and the record says for how many processes it took.
 */
export function holdFullSpeed(pids: Iterable<number>, platform: NodeJS.Platform = process.platform): HighQos | null {
  if (platform !== "win32") {
    return null;
  }
  const list = [...pids];
  if (list.length === 0) {
    return { processes: 0, failed: [] };
  }
  const script = `$PIDS = @(${list.join(",")})\n${HIGH_QOS_SCRIPT}`;
  const shell = spawnSync(
    "powershell.exe",
    ["-NoProfile", "-NonInteractive", "-EncodedCommand", Buffer.from(script, "utf16le").toString("base64")],
    { encoding: "utf8", windowsHide: true },
  );
  if (shell.status !== 0) {
    throw new Error(`switching power throttling off failed (${shell.status}): ${shell.stderr}`);
  }
  const failed: { pid: number; error: number }[] = [];
  let processes = 0;
  for (const line of shell.stdout.split(/\r?\n/)) {
    const found = /^(\d+) (\d+)$/.exec(line.trim());
    if (found === null) continue;
    const error = Number(found[2]);
    if (error === 0) {
      processes += 1;
    } else {
      failed.push({ pid: Number(found[1]), error });
    }
  }
  return { processes, failed };
}

/** The per-processor times `os.cpus()` gives, summed: busy (user, nice, sys, irq) and all. */
export interface CpuTimes {
  readonly busyMs: number;
  readonly totalMs: number;
}

export function sumTimes(list: readonly { times: { user: number; nice: number; sys: number; idle: number; irq: number } }[]): CpuTimes {
  let busyMs = 0;
  let totalMs = 0;
  for (const { times } of list) {
    const busy = times.user + times.nice + times.sys + times.irq;
    busyMs += busy;
    totalMs += busy + times.idle;
  }
  return { busyMs, totalMs };
}

/** Logical processors busy between two samples of `count` processors, to two decimals. */
export function busyCores(before: CpuTimes, after: CpuTimes, count: number): number {
  const total = after.totalMs - before.totalMs;
  if (total <= 0) {
    return 0;
  }
  return Number((((after.busyMs - before.busyMs) / total) * count).toFixed(2));
}

export type LoadKind = "loadavg" | "busy-cores";

export function loadKind(platform: NodeJS.Platform = process.platform): LoadKind {
  return platform === "win32" ? "busy-cores" : "loadavg";
}

let lastTimes: CpuTimes = sumTimes(cpus());

/**
 * The host load now, first element read: the macOS load averages, or on Windows the processors
 * busy since the previous call (the first counting from module load).
 */
export function hostLoad(platform: NodeJS.Platform = process.platform): number[] {
  if (loadKind(platform) === "loadavg") {
    return loadavg().map((n) => Number(n.toFixed(2)));
  }
  const list = cpus();
  const now = sumTimes(list);
  const busy = busyCores(lastTimes, now, list.length);
  lastTimes = now;
  return [busy];
}
