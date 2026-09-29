import { describe, expect, test } from "bun:test";
import { browserProcesses, busyCores, cpuOf, holdFullSpeed, loadKind, parseTable, parseWindowsTable, sumTimes } from "../../tests/processCpu";

describe("the Windows process table", () => {
  // The shape `ConvertTo-Json -Compress` gives `Win32_Process` rows: numbers, a null command line
  // for a process the caller may not read, and 100 ns units for both times.
  const json = JSON.stringify([
    { ProcessId: 4, ParentProcessId: 0, KernelModeTime: 900_000_000, UserModeTime: 0, CommandLine: null },
    { ProcessId: 100, ParentProcessId: 1, KernelModeTime: 2_343_750, UserModeTime: 6_250_000, CommandLine: "node worker.js" },
    { ProcessId: 200, ParentProcessId: 100, KernelModeTime: 10_000_000, UserModeTime: 20_000_000, CommandLine: '"C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe" --headless' },
    { ProcessId: 201, ParentProcessId: 200, KernelModeTime: 5_000_000, UserModeTime: 5_000_000, CommandLine: '"C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe" --type=renderer' },
  ]);

  test("CPU seconds are kernel plus user time in 100 ns units", () => {
    const table = parseWindowsTable(json);
    expect(table.map((p) => p.pid)).toEqual([4, 100, 200, 201]);
    expect(table[1]?.cpuS).toBeCloseTo(0.859375, 9);
    expect(table[0]?.command).toBe("");
  });

  test("one process is an object, not a list, and is still read", () => {
    const one = parseWindowsTable(JSON.stringify({ ProcessId: 7, ParentProcessId: 1, KernelModeTime: 1e7, UserModeTime: 0, CommandLine: "x" }));
    expect(one).toEqual([{ pid: 7, ppid: 1, cpuS: 1, command: "x" }]);
  });

  test("a row without a process id throws rather than counting as pid NaN", () => {
    expect(() => parseWindowsTable(JSON.stringify([{ ParentProcessId: 1 }]))).toThrow("does not parse");
  });

  test("the browser is the worker's descendants, as on macOS", () => {
    const table = parseWindowsTable(json);
    const found = browserProcesses(table, 100, "chromium", "");
    expect([...found.pids].sort()).toEqual([200, 201]);
    expect(found.shared).toBeNull();
    expect(cpuOf(table, found.pids)).toBeCloseTo(4, 9);
  });
});

describe("the macOS process table", () => {
  test("`ps` time columns parse as before", () => {
    const table = parseTable("  10     1   1:02.50 /usr/bin/thing --flag\n 11 10 01-00:00:01.00 child\n");
    expect(table.map((p) => [p.pid, p.ppid, p.cpuS])).toEqual([
      [10, 1, 62.5],
      [11, 10, 86_401],
    ]);
  });
});

describe("power throttling", () => {
  test("is switched off on Windows only; elsewhere there is nothing to switch", () => {
    expect(holdFullSpeed([1, 2, 3], "darwin")).toBeNull();
  });

  test("an empty process list asks Windows for nothing", () => {
    expect(holdFullSpeed([], "win32")).toEqual({ processes: 0, failed: [] });
  });
});

describe("the host load", () => {
  test("Windows has busy processors, macOS its load average", () => {
    expect(loadKind("win32")).toBe("busy-cores");
    expect(loadKind("darwin")).toBe("loadavg");
  });

  test("busy processors are the busy share of all processor time times the processor count", () => {
    const cpu = (user: number, idle: number) => ({ times: { user, nice: 0, sys: 0, idle, irq: 0 } });
    const before = sumTimes([cpu(0, 0), cpu(0, 0), cpu(0, 0), cpu(0, 0)]);
    // Two of four processors fully busy for 1 s, two idle.
    const after = sumTimes([cpu(1000, 0), cpu(1000, 0), cpu(0, 1000), cpu(0, 1000)]);
    expect(busyCores(before, after, 4)).toBe(2);
    expect(busyCores(after, after, 4)).toBe(0);
  });
});
