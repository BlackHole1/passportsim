// The BLE card. The state line is read, not guessed: while open, the card asks `ble_scan` for a
// zero-length read through the unjournaled reader, and every answer or refusal updates it too.

import { BluetoothConnectedIcon, BluetoothIcon, BluetoothOffIcon, BluetoothSearchingIcon } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { attempt } from "../../controllers";
import type { MessageKey } from "../../i18n";
import * as ble from "../../panels/ble";
import { Badge } from "../../../ui/badge";
import { cn } from "../../../ui/lib/utils";
import { Textarea } from "../../../ui/textarea";
import { Action, Actions, ErrorLine, Field, Note, NumberInput, Readout, Select, fieldId } from "../controls";
import { usePage, useStore, useT } from "../hooks";
import { RadioUnbound } from "../notices";

const READ_EVERY_MS = 1_000;

function stateWord(radio: ble.RadioState): string {
  switch (radio.kind) {
    case "advertising":
      return radio.adv.connectable ? "advertising-connectable" : "advertising-non-connectable";
    default:
      return radio.kind;
  }
}

function unboundReason(radio: ble.RadioState): MessageKey | null {
  if (radio.kind !== "not_bound") {
    return null;
  }
  switch (radio.binding) {
    case "not linked":
      return "ble.unbound.notLinked";
    case "unsupported image":
      return radio.elf ? "ble.unbound.refused" : "ble.unbound.noElf";
    case "disabled":
      return "ble.unbound.disabled";
    default:
      return null;
  }
}

const WHY_KEY: Record<ble.Why["why"], MessageKey> = {
  non_connectable: "ble.why.nonConnectable",
  not_connected: "ble.why.notConnected",
  not_advertising: "ble.why.notAdvertising",
  peer_not_seen: "ble.why.peerNotSeen",
  no_answer: "ble.why.noAnswer",
};

export function BleCard() {
  const page = usePage();
  const t = useT();
  const client = page.ctx.client;
  const reader = page.ctx.reader;
  const generation = useStore(page.generation);
  const narrow = useStore(page.layout).narrow;
  useStore(page.tabsVersion);
  // Read only while visible: the workbench is mounted but hidden in simple mode.
  const open = useStore(page.prefs).mode === "advanced" && !page.tabs.isCollapsed("ble", narrow);
  const [state, setState] = useState(ble.DEFAULT_BLE);
  const current = useRef(ble.DEFAULT_BLE);
  const [radio, setRadio] = useState<ble.RadioState>(ble.UNKNOWN_RADIO);
  const [scan, setScan] = useState<ble.ScanOutcome | null>(null);
  const [why, setWhy] = useState<ble.Why | null>(null);
  const [target, setTarget] = useState<string>("");
  const targetRef = useRef("");
  const value = useRef<HTMLTextAreaElement>(null);
  const [error, setError] = useState<string | null>(null);
  const id = (name: string) => fieldId("ble", name);
  const keep = (next: ble.BleState) => {
    current.current = next;
    setState(next);
  };
  const learn = (json: unknown) => {
    const next = ble.radioFromResult(json);
    if (next !== null) {
      setRadio(next);
    }
  };

  const refresh = async () => {
    if (reader === undefined) {
      return;
    }
    try {
      learn((await reader.call("ble_scan", ble.READ_ARGS)).json);
    } catch (cause) {
      const named = ble.radioFromRefusal(cause);
      if (named !== null) {
        setRadio(named);
      }
    }
  };

  useEffect(() => {
    keep(ble.DEFAULT_BLE);
    setRadio(ble.UNKNOWN_RADIO);
    setScan(null);
    setWhy(null);
    setError(null);
  }, [generation]);

  useEffect(() => {
    if (!open || reader === undefined || generation === 0) {
      return;
    }
    void refresh();
    const timer = setInterval(() => {
      if (document.visibilityState === "visible") {
        void refresh();
      }
    }, READ_EVERY_MS);
    return () => {
      clearInterval(timer);
    };
  }, [open, generation, reader]);

  /**
   * Runs one control. A refusal naming the radio's state or a reason becomes the card's state line,
   * with the raw text in the page's log; any other refusal is the error line.
   */
  const run = async (command: string, work: () => Promise<void>) => {
    setWhy(null);
    setError(null);
    const done = await attempt(work);
    if (!done.ok) {
      const named = ble.radioFromRefusal(done.cause);
      const reason = ble.whyOf(done.cause);
      if (named !== null || reason !== null) {
        if (named !== null) {
          setRadio(named);
        }
        setWhy(reason);
        page.actions.logRefusal(command, done.error);
      } else {
        setError(done.error);
      }
    }
    void refresh();
  };

  // The chooser keeps the user's pick while a call is in flight, so a write cannot land on a
  // read-only characteristic the last call left selected.
  const options = ble.characteristics(state.tree);
  const uuids = options.map((node) => node.uuid);
  const chosen = uuids.includes(target) ? target : state.selected !== null && uuids.includes(state.selected) ? state.selected : (uuids[0] ?? "");
  targetRef.current = chosen;

  /**
   * Lets the guest run and collects notifications, up to `POLL_TRIES` journaled polls, so a replay
   * reproduces the wait; stops at the first poll that brought something new.
   */
  const collect = async () => {
    for (let attempt = 0; attempt < ble.POLL_TRIES; attempt += 1) {
      const result = await client.call("ble_gatt", ble.notificationsArgs(current.current.subscribed, ble.POLL_SETTLE_MS));
      keep({ ...current.current, notifications: ble.notificationsFromResult(result.json) });
      if (ble.newCount(result.json) > 0) {
        return;
      }
    }
  };

  const link =
    state.connected === null
      ? t("ble.notConnected")
      : state.subscribed === null
        ? t("ble.connected", { addr: state.connected })
        : t("ble.subscribed", { addr: state.connected, uuid: state.subscribed });

  const unbound = unboundReason(radio);
  const peers = state.peers;

  return (
    <>
      {radio.kind === "not_bound" ? (
        <div className="flex flex-col gap-1.5" data-ble-state="not_bound">
          <RadioUnbound radio="ble" />
          {unbound === null ? null : <p className="text-muted-foreground text-xs">{t(unbound)}</p>}
        </div>
      ) : (
        <RadioLine radio={radio} />
      )}
      <Field hint="ms" id={id("scan")} label={t("ble.scan")}>
        <NumberInput
          id={id("scan")}
          max={30_000}
          min={1}
          onCommit={(scanMs) => {
            keep({ ...current.current, scanMs });
          }}
          value={state.scanMs}
        />
      </Field>
      <Actions>
        <Action
          onClick={() =>
            run("ble_scan", async () => {
              const result = await client.call("ble_scan", ble.scanArgs(current.current.scanMs));
              keep({ ...current.current, peers: ble.heardPeers(ble.peersFromResult(result.json)) });
              setScan(ble.scanOutcomeFromResult(result.json));
              learn(result.json);
            })
          }
          variant="default"
        >
          {t("ble.scanRun")}
        </Action>
        <Action
          onClick={() =>
            run("ble_gatt", async () => {
              const result = await client.call("ble_gatt", ble.gattArgs("discover"));
              const found = ble.treeFromResult(result.json);
              const first = ble.characteristics(found)[0];
              keep({ ...current.current, tree: found, selected: current.current.selected ?? first?.uuid ?? null });
              learn(result.json);
            })
          }
        >
          {t("ble.discover")}
        </Action>
        <Action
          disabled={state.connected === null}
          onClick={() =>
            run("ble_connect", async () => {
              const connected = current.current.connected;
              if (connected === null) {
                return;
              }
              const args = ble.disconnectArgs(connected);
              const result = await client.call("ble_connect", args);
              keep(ble.fromConnectArgs(args, current.current));
              learn(result.json);
            })
          }
        >
          {t("ble.disconnect")}
        </Action>
      </Actions>
      {scan === null || scan.outcome === "read" ? null : (
        <p className="ble-scan text-muted-foreground text-xs" data-ble-scan={scan.outcome} role="status">
          {scan.outcome === "heard"
            ? t("ble.scan.heard", { count: String(scan.heard) })
            : scan.outcome === "silent"
              ? t("ble.scan.silent")
              : t("ble.scan.noneMatching", { events: String(scan.advEvents) })}
        </p>
      )}
      <ul aria-label={t("ble.peers.aria")} className="peer-list flex flex-col gap-1 text-xs empty:hidden">
        {peers.map((peer) => (
          <li
            className={cn("flex flex-wrap items-center gap-x-2 gap-y-1 rounded-md bg-muted px-2 py-1", !ble.isPlaceholderAddr(peer.addr) && "foreign")}
            data-connectable={peer.connectable === true ? "true" : "false"}
            data-pdu={peer.pdu ?? ""}
            key={peer.addr}
          >
            <span className="flex min-w-[8rem] flex-1 flex-col">
              <span className="truncate font-medium">{peer.name === ble.NO_NAME ? t("ble.unnamed") : peer.name}</span>
              <span className="truncate font-mono text-muted-foreground">{peer.addr}</span>
            </span>
            {peer.pdu ? (
              <Badge className="font-mono" size="sm" variant="outline">
                {peer.pdu}
              </Badge>
            ) : null}
            <Badge data-ble-marker="" size="sm" variant={peer.connectable === true ? "secondary" : "outline"}>
              {peer.connectable === true ? t("ble.peer.connectable") : t("ble.peer.nonConnectable")}
            </Badge>
            {peer.connectable === true && state.connected !== peer.addr ? (
              <Action
                onClick={() =>
                  run("ble_connect", async () => {
                    const result = await client.call("ble_connect", ble.connectArgs(peer.addr));
                    keep(ble.fromConnectArgs(ble.connectArgs(peer.addr), current.current));
                    learn(result.json);
                  })
                }
                variant="ghost"
              >
                {t("ble.connect")}
              </Action>
            ) : null}
          </li>
        ))}
      </ul>
      {why === null ? null : (
        <p
          className="ble-why rounded-md border bg-muted px-2.5 py-1.5 text-foreground text-xs leading-relaxed"
          data-ble-why={why.why}
          role="status"
        >
          {why.why === "non_connectable" ? t(WHY_KEY[why.why], { addr: why.addr, pdu: why.pdu }) : t(WHY_KEY[why.why])}
        </p>
      )}
      <ul aria-label={t("ble.tree.aria")} className="gatt-tree flex flex-col font-mono text-xs empty:hidden">
        {ble.flattenTree(state.tree).map((row) => (
          <li key={`${row.node.handle}-${row.node.uuid}`} style={{ paddingLeft: row.depth * 12 }}>
            {`${row.node.kind} ${row.node.uuid} @${row.node.handle}${row.node.properties ? ` (${row.node.properties})` : ""}`}
          </li>
        ))}
      </ul>
      <Readout data-ble="link">{link}</Readout>
      <Field id={id("characteristic")} label={t("ble.characteristic")}>
        <Select
          controlled
          disabled={options.length === 0}
          id={id("characteristic")}
          onChange={setTarget}
          options={options.map((node) => ({ value: node.uuid, label: `${node.uuid}${node.properties ? ` (${node.properties})` : ""}` }))}
          value={chosen}
        />
      </Field>
      <Field id={id("value")} label={t("ble.value")}>
        <Textarea aria-describedby={id("value-hint")} id={id("value")} ref={value} rows={2} spellCheck={false} />
      </Field>
      {/* Under the box rather than beside it: beside a 400 px card's text box the sentence is cut. */}
      <p className="ps-[7.25rem] text-muted-foreground text-xs" id={id("value-hint")}>
        {t("ble.valueHint")}
      </p>
      <Actions>
        <Action
          onClick={() =>
            run("ble_gatt", async () => {
              const uuid = targetRef.current;
              await client.call("ble_gatt", ble.subscribeArgs(uuid));
              keep({ ...current.current, subscribed: uuid });
            })
          }
        >
          {t("ble.subscribe")}
        </Action>
        <Action
          onClick={() =>
            run("ble_gatt", async () => {
              const uuid = targetRef.current;
              await client.call("ble_gatt", ble.writeTextArgs(uuid, value.current?.value ?? ""));
              await collect();
            })
          }
          variant="default"
        >
          {t("ble.write")}
        </Action>
      </Actions>
      <ul aria-label={t("ble.notifications.aria")} className="notification-list flex flex-col gap-1 font-mono text-xs empty:hidden">
        {state.notifications.map((one, index) => (
          <li key={index}>{`${one.indication ? "indication" : "notification"} ${one.text ?? one.value}`}</li>
        ))}
      </ul>
      <Note>{t("ble.note")}</Note>
      <ErrorLine error={error} />
    </>
  );
}

function RadioLine(props: { readonly radio: ble.RadioState }) {
  const t = useT();
  const { radio } = props;
  const name = (adv: ble.Advertising) => adv.name ?? t("ble.unnamed");
  let text: string;
  let Icon = BluetoothIcon;
  let tone = "text-muted-foreground";
  switch (radio.kind) {
    case "unknown":
      text = t("ble.state.unknown");
      break;
    case "not_bound":
      text = t("radio.unbound.ble");
      Icon = BluetoothOffIcon;
      break;
    case "not_started":
      text = t("ble.state.notStarted");
      Icon = BluetoothOffIcon;
      tone = "text-foreground";
      break;
    case "stopped":
      text = t("ble.state.stopped");
      Icon = BluetoothOffIcon;
      tone = "text-foreground";
      break;
    case "idle":
      text = t("ble.state.idle");
      break;
    case "advertising":
      text = radio.adv.connectable
        ? t("ble.state.connectable", { name: name(radio.adv), pdu: radio.adv.pdu })
        : t("ble.state.nonConnectable", { name: name(radio.adv), pdu: radio.adv.pdu });
      Icon = BluetoothSearchingIcon;
      tone = "text-foreground";
      break;
    case "connected":
      text = t("ble.state.connected");
      Icon = BluetoothConnectedIcon;
      tone = "text-foreground";
      break;
  }
  return (
    <div className="ble-state flex items-start gap-2 rounded-lg border bg-muted/50 px-3 py-2" data-ble-state={stateWord(radio)} role="status">
      <Icon aria-hidden="true" className={cn("mt-0.5 size-4 shrink-0", tone)} />
      <p className="min-w-0 text-sm leading-snug">{text}</p>
    </div>
  );
}
