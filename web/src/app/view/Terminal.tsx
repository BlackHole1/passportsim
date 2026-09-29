// The console's lines as a scrolling monospace box. Not xterm.js: the lines are cursor-linked and
// filtered, which a terminal's cell grid has nowhere to hold.

import { ArrowDownIcon } from "lucide-react";
import { useLayoutEffect, useRef, useState } from "react";
import { cn } from "../../ui/lib/utils";
import { lineStamp, type ConsoleLine } from "../console";
import { useT } from "./hooks";

/** How many rows are rendered; the model keeps more. */
export const RENDERED_ROWS = 800;

export const BOTTOM_SLACK_PX = 24;

export function atBottom(node: { scrollHeight: number; scrollTop: number; clientHeight: number }): boolean {
  return node.scrollHeight - node.scrollTop - node.clientHeight < BOTTOM_SLACK_PX;
}

export function ConsoleRow(props: { readonly line: ConsoleLine; readonly tagStream?: boolean }) {
  const { line } = props;
  if (line.gap) {
    return <div className="terminal-line terminal-gap text-muted-foreground italic">{line.text}</div>;
  }
  return (
    <div className="terminal-line" data-cursor={String(line.cursor)} data-stream={line.stream}>
      <span className="terminal-cursor select-none text-muted-foreground/70">{`${lineStamp(line)} `}</span>
      {props.tagStream ? (
        <span className="terminal-stream select-none text-muted-foreground/70">{`${line.stream.padEnd(5)} `}</span>
      ) : null}
      {line.text}
      {line.repeat > 1 ? <span className="text-muted-foreground">{` x${line.repeat}`}</span> : null}
    </div>
  );
}

/**
 * A scrolling log. With `follow` on, new lines scroll to the bottom; scrolling up pauses that
 * without changing the setting and shows the jump button. With `follow` off, the browser's scroll
 * anchoring keeps the lines being read in place while old ones are trimmed above.
 */
export function Terminal(props: {
  readonly lines: readonly ConsoleLine[];
  readonly label: string;
  readonly className?: string;
  readonly rows?: number;
  readonly version: unknown;
  readonly tagStream?: boolean;
  readonly follow: boolean;
}) {
  const t = useT();
  const box = useRef<HTMLDivElement>(null);
  // Also a ref, so the layout effect reads this render's scroll, not the last rendered one.
  const bottom = useRef(true);
  const [away, setAway] = useState(false);
  const followed = useRef(props.follow);
  useLayoutEffect(() => {
    const node = box.current;
    if (!node) {
      return;
    }
    if (props.follow && !followed.current) {
      bottom.current = true;
    }
    followed.current = props.follow;
    if (props.follow && bottom.current) {
      node.scrollTop = node.scrollHeight;
    }
    bottom.current = atBottom(node);
    setAway(!bottom.current);
  }, [props.version, props.follow]);
  const rows = props.rows ?? RENDERED_ROWS;
  const tail = props.lines.slice(Math.max(0, props.lines.length - rows));
  const jump = () => {
    const node = box.current;
    if (node) {
      node.scrollTop = node.scrollHeight;
      bottom.current = true;
      setAway(false);
    }
  };
  return (
    <div className={cn("terminal-frame relative flex min-h-0 flex-col", props.className)}>
      <div
        aria-label={props.label}
        aria-live="polite"
        className="terminal min-h-0 flex-1 overflow-auto whitespace-pre-wrap font-mono [overflow-wrap:anywhere] text-[0.75rem] leading-5"
        data-scroller=""
        onScroll={(event) => {
          bottom.current = atBottom(event.currentTarget);
          setAway(!bottom.current);
        }}
        ref={box}
        role="log"
        tabIndex={0}
      >
        {tail.map((line, index) => (
          // Keyed by cursor, unique per stream; a gap row has none of its own.
          <ConsoleRow
            key={line.gap ? `gap-${index}` : `${line.stream}-${String(line.cursor)}`}
            line={line}
            tagStream={props.tagStream}
          />
        ))}
      </div>
      {away ? (
        <button
          className="terminal-jump absolute end-3 bottom-2 inline-flex items-center gap-1 rounded-full border bg-popover px-2.5 py-1 font-sans text-popover-foreground text-xs shadow-sm/5 outline-none hover:bg-accent focus-visible:ring-2 focus-visible:ring-ring"
          data-follow-paused={props.follow ? "true" : "false"}
          onClick={jump}
          type="button"
        >
          <ArrowDownIcon aria-hidden="true" className="size-3.5" />
          {props.follow ? t("log.paused") : t("log.jump")}
        </button>
      ) : null}
    </div>
  );
}

export function FollowToggle(props: { readonly id: string; readonly checked: boolean; readonly onChange: (on: boolean) => void }) {
  const t = useT();
  return (
    <label className="inline-flex items-center gap-1.5 whitespace-nowrap text-muted-foreground text-xs" htmlFor={props.id}>
      <input
        checked={props.checked}
        className="size-3.5 accent-foreground"
        data-follow=""
        id={props.id}
        onChange={(event) => {
          props.onChange(event.currentTarget.checked);
        }}
        type="checkbox"
      />
      {t("log.follow")}
    </label>
  );
}
