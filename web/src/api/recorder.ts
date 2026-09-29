// The scenario recorder: a window on the `UiJournal`, rendered through the same `redact.ts` and
// `step.ts` as "Copy as scenario step".
//
// A step is an accepted call that changes the machine or its world and has a step form. Refused,
// unanswered and read-only calls, and `clock`, are listed in the header comment with the reason.
//
// Waits come from the machine's event ring. The gap to the next call starts after the virtual time
// the call itself advanced. Within it, the first event with an `event:` matcher becomes
// `wait: 'event:<kind>'` and the rest `delay`; otherwise the whole gap is a `delay`. Page stamps
// lag the machine by up to one slice, so an event goes to the latest call stamped before it.

import type { CommandName } from "./commands";
import type { Json } from "./envelope";
import type { JournalRecord, UiJournal } from "./journal";
import { redactCall, type SecretValues } from "./redact";
import { SCENARIO_SCHEMA } from "./registryShape";
import { renderStep, stepOf, yamlFlow, yamlString, type StepResult } from "./step";
import { EventKind } from "../worker/layout";

export type UsbWorld = "unplugged" | "charger" | "host" | "open";

export interface SessionStart {
  readonly image: string | null;
  readonly power: "on" | "off";
  /** `setup.usb`, applied by the runner through `env` before step 1; `null` leaves it out. */
  readonly usb: UsbWorld | null;
}

export interface RecordedEvent {
  readonly kind: number;
  readonly vtPs: bigint;
}

/**
 * The machine event kinds with an `event:` matcher, in the order a wait prefers them. `UiSettled`
 * is read as `ui_changed` (unverified that they fire on exactly the same generations). `Frame` is
 * left out (nearly every slice has one) and so is `Panic` (a recording would turn a crash into a pass).
 */
const WAIT_EVENTS: ReadonlyArray<readonly [number, string]> = [
  [EventKind.UiSettled, "ui_changed"],
  [EventKind.Reset, "reset"],
  [EventKind.Sleep, "sleep"],
];

const PS_PER_MS = 1_000_000_000n;
const PS_PER_US = 1_000_000n;

export function durationOf(ps: bigint): string | null {
  if (ps < PS_PER_US) {
    return null;
  }
  return ps % PS_PER_MS === 0n ? `${ps / PS_PER_MS}ms` : `${ps / PS_PER_US}us`;
}

function readOnly(command: CommandName, args: Json): boolean {
  const op = typeof args === "object" && args !== null && !Array.isArray(args) ? args.op : undefined;
  switch (command) {
    case "status":
    case "doctor":
    case "ui":
    case "inspect":
      return true;
    case "serial":
      return op !== "write";
    case "snapshot":
      return op === "list";
    default:
      return false;
  }
}

export interface RecordedScenario {
  readonly yaml: string;
  readonly steps: number;
  readonly notes: readonly string[];
}

export interface ExportInput {
  readonly name: string;
  readonly description?: string;
  readonly start: SessionStart;
  readonly calls: readonly JournalRecord[];
  readonly events: readonly RecordedEvent[];
  readonly endPs: bigint | null;
  readonly lost: number;
  readonly secrets: SecretValues;
}

export function exportScenario(input: ExportInput): RecordedScenario {
  const notes: string[] = [];
  if (input.lost > 0) {
    notes.push(
      `${input.lost} earlier calls of this session fell off the journal (JOURNAL_LIMIT) and are not in this file, so it does not replay the whole session`,
    );
  }
  const secrets = input.secrets;
  const kept: Array<{ record: JournalRecord; step: Extract<StepResult, { ok: true }> }> = [];
  const skipped = new Map<string, number>();
  const skip = (reason: string) => skipped.set(reason, (skipped.get(reason) ?? 0) + 1);
  const redactedPaths = new Set<string>();

  for (const record of input.calls) {
    if (record.outcome.state !== "ok") {
      skip(`\`${record.command}\`: ${record.outcome.state === "failed" ? "refused by the registry" : "never answered"}`);
      continue;
    }
    if (readOnly(record.command, record.args)) {
      skip(`\`${record.command}\`: read-only`);
      continue;
    }
    const redacted = redactCall(record.command, record.args, secrets);
    if (redacted.refused !== null) {
      skip(redacted.refused);
      continue;
    }
    const step = stepOf(record.command, redacted.args);
    if (!step.ok) {
      skip(step.reason);
      continue;
    }
    for (const path of redacted.redacted) {
      redactedPaths.add(`${record.command} ${path}`);
    }
    kept.push({ record, step });
  }
  for (const [reason, count] of skipped) {
    notes.push(`skipped ${count}x: ${reason}`);
  }
  if (redactedPaths.size > 0) {
    notes.push(`secrets replaced by <SECRET>: ${[...redactedPaths].join(", ")}`);
  }

  const lines: string[] = [];
  const push = (step: Extract<StepResult, { ok: true }>, name?: string) => {
    lines.push(renderStep(step, { indent: 2, name }));
  };
  const events = [...input.events].sort((a, b) => (a.vtPs < b.vtPs ? -1 : a.vtPs > b.vtPs ? 1 : 0));
  let unstamped = false;
  kept.forEach(({ record, step }, index) => {
    push(step, `${record.command} (journal #${record.seq})`);
    const from = record.vtPs;
    const to = kept[index + 1]?.record.vtPs ?? input.endPs;
    if (from === null || to === null) {
      unstamped ||= from === null;
      return;
    }
    // The step replays the time its own call advanced, so the gap to fill starts after it.
    const own = record.outcome.state === "ok" && typeof record.outcome.elapsedVtUs === "number" ? BigInt(record.outcome.elapsedVtUs) * PS_PER_US : 0n;
    let at = from + own > to ? to : from + own;
    const event = WAIT_EVENTS.map(([kind, name]) => ({
      name,
      hit: events.find((e) => e.kind === kind && e.vtPs > from && e.vtPs <= to),
    })).find((candidate) => candidate.hit !== undefined);
    if (event?.hit !== undefined) {
      push({ ok: true, key: "wait", value: `event:${event.name}`, timeout: null }, `the ${event.name} that followed`);
      at = event.hit.vtPs > at ? event.hit.vtPs : at;
    }
    const gap = durationOf(to - at);
    if (gap !== null) {
      push({ ok: true, key: "delay", value: gap, timeout: null });
    }
  });
  if (unstamped) {
    notes.push("some calls were made before a machine reported virtual time, so no wait or delay follows them");
  }

  const setup: { [key: string]: Json } = { power: input.start.power };
  if (input.start.usb !== null) {
    setup.usb = input.start.usb;
  }
  const header = [
    "# Recorded in the PassportSim web UI. Review before committing it under tests/scenarios/.",
    ...notes.map((note) => `# ${note.replace(/\r?\n/g, " ")}`),
    `schema: ${SCENARIO_SCHEMA}`,
    `name: ${yamlString(input.name)}`,
  ];
  if (input.description !== undefined && input.description !== "") {
    header.push(`description: ${yamlString(input.description)}`);
  }
  if (input.start.image !== null) {
    header.push(`image: ${yamlString(input.start.image)}`);
  }
  header.push(`setup: ${yamlFlow(setup)}`);
  const body = lines.length === 0 ? ["steps: []"] : ["steps:", ...lines];
  return { yaml: `${[...header, ...body].join("\n")}\n`, steps: lines.length, notes };
}

export class ScenarioRecorder {
  private session: { firstSeq: number; start: SessionStart; events: RecordedEvent[] } | null = null;

  constructor(private readonly journal: UiJournal) {}

  get recording(): boolean {
    return this.session !== null;
  }

  start(start: SessionStart): void {
    this.session = { firstSeq: (this.journal.last()?.seq ?? 0) + 1, start, events: [] };
  }

  observe(events: readonly RecordedEvent[]): void {
    this.session?.events.push(...events.map((e) => ({ kind: e.kind, vtPs: e.vtPs })));
  }

  stop(name: string, endPs: bigint | null, description?: string): RecordedScenario | null {
    const session = this.session;
    if (session === null) {
      return null;
    }
    this.session = null;
    const calls = this.journal.list().filter((record) => record.seq >= session.firstSeq);
    const oldest = this.journal.list()[0]?.seq ?? session.firstSeq;
    return exportScenario({
      name,
      description,
      start: session.start,
      calls,
      events: session.events,
      endPs,
      lost: Math.max(0, oldest - session.firstSeq),
      secrets: this.journal.secrets,
    });
  }
}
