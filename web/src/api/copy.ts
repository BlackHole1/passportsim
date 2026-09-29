// "Copy as CLI" and "Copy as scenario step". Both redact against the journal's session-long secret
// set first, so a value masked in one entry is masked wherever else it was typed.

import { cliCall, renderCli, type Shell } from "./argv";
import type { JournalRecord } from "./journal";
import { redactCall, type SecretValues } from "./redact";
import { REGISTRY_SHAPE, UNREGISTERED } from "./registryShape";
import { renderStep, stepOf } from "./step";

export type Copied =
  | { readonly ok: true; readonly text: string; readonly redacted: readonly string[] }
  | { readonly ok: false; readonly reason: string };

function redacted(record: JournalRecord, secrets: SecretValues) {
  return redactCall(record.command, record.args, secrets);
}

export function copyAsCli(record: JournalRecord, secrets: SecretValues, shell: Shell): Copied {
  // A journal entry can name a command a later build registered or renamed; say so rather than throw.
  if (!(REGISTRY_SHAPE[record.command] ?? UNREGISTERED).registered) {
    return {
      ok: false,
      reason: `\`${record.command}\` is not a registered command yet, so \`passportsim\` has no subcommand for it`,
    };
  }
  const call = redacted(record, secrets);
  if (call.refused !== null) {
    return { ok: false, reason: call.refused };
  }
  return { ok: true, text: renderCli(cliCall(record.command, call.args), shell), redacted: call.redacted };
}

/** "Copy as scenario step" for one entry: a `steps:` item, OS-neutral. */
export function copyAsStep(record: JournalRecord, secrets: SecretValues): Copied {
  const call = redacted(record, secrets);
  if (call.refused !== null) {
    return { ok: false, reason: call.refused };
  }
  const step = stepOf(record.command, call.args);
  if (!step.ok) {
    return { ok: false, reason: step.reason };
  }
  return { ok: true, text: renderStep(step), redacted: call.redacted };
}
