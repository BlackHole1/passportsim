// Form controls the cards share. Ids are `f-<card>-<field>` in fixed words, so Playwright finds
// the same id in every language. Committed values listen to native `change` (see
// `useNativeChange`).

import { ChevronDownIcon } from "lucide-react";
import type { ReactNode } from "react";
import { Button } from "../../ui/button";
import { cn } from "../../ui/lib/utils";
import { useNativeChange } from "./hooks";

export function fieldId(ns: string, name: string): string {
  return `f-${ns}-${name}`;
}

export function Field(props: { readonly id: string; readonly label: string; readonly hint?: string; readonly children: ReactNode }) {
  return (
    <div className="field grid grid-cols-[6.5rem_minmax(0,1fr)] items-center gap-x-3 gap-y-1">
      <label className="truncate text-muted-foreground text-sm" htmlFor={props.id}>
        {props.label}
      </label>
      <div className="flex min-w-0 items-center gap-2">
        {props.children}
        {props.hint ? <span className="hint shrink-0 text-muted-foreground text-xs">{props.hint}</span> : null}
      </div>
    </div>
  );
}

const INPUT_SHELL =
  "relative inline-flex w-full min-w-0 rounded-lg border border-input bg-background not-dark:bg-clip-padding text-base shadow-xs/5 ring-ring/24 transition-shadow has-focus-visible:border-ring has-focus-visible:ring-[3px] has-disabled:opacity-64 sm:text-sm dark:bg-input/32";
const INPUT_INNER =
  "h-8.5 w-full min-w-0 rounded-[inherit] bg-transparent px-[calc(--spacing(3)-1px)] text-foreground leading-8.5 outline-none placeholder:text-muted-foreground/72 sm:h-7.5 sm:leading-7.5";

export function NumberInput(props: {
  readonly id: string;
  readonly value: number;
  readonly min: number;
  readonly max: number;
  readonly step?: number;
  readonly onCommit: (value: number) => void;
}) {
  const ref = useNativeChange<HTMLInputElement>((node) => {
    props.onCommit(Number(node.value));
  });
  return (
    <span className={INPUT_SHELL}>
      <input
        className={INPUT_INNER}
        defaultValue={String(props.value)}
        id={props.id}
        max={props.max}
        min={props.min}
        ref={ref}
        step={props.step ?? 1}
        type="number"
      />
    </span>
  );
}

export function TextInput(props: {
  readonly id: string;
  readonly value: string;
  readonly placeholder?: string;
  readonly autoComplete?: string;
  readonly onCommit: (value: string) => void;
}) {
  const ref = useNativeChange<HTMLInputElement>((node) => {
    props.onCommit(node.value);
  });
  return (
    <span className={INPUT_SHELL}>
      <input
        autoComplete={props.autoComplete ?? "off"}
        className={INPUT_INNER}
        defaultValue={props.value}
        id={props.id}
        placeholder={props.placeholder}
        ref={ref}
        spellCheck={false}
        type="text"
      />
    </span>
  );
}

/** A slider; `onCommit` runs on `change`, once per drag, never per pixel. */
export function RangeInput(props: {
  readonly id: string;
  readonly value: number;
  readonly min: number;
  readonly max: number;
  readonly onCommit: (value: number) => void;
}) {
  const ref = useNativeChange<HTMLInputElement>((node) => {
    props.onCommit(Number(node.value));
  });
  return (
    <input
      className="h-7 w-full min-w-0 cursor-pointer accent-foreground"
      defaultValue={String(props.value)}
      id={props.id}
      max={props.max}
      min={props.min}
      ref={ref}
      step={1}
      type="range"
    />
  );
}

export function Checkbox(props: {
  readonly id: string;
  readonly checked: boolean;
  readonly disabled?: boolean;
  readonly onChange: (checked: boolean) => void;
}) {
  return (
    <input
      className="size-4 cursor-pointer accent-foreground disabled:cursor-not-allowed disabled:opacity-64"
      defaultChecked={props.checked}
      disabled={props.disabled}
      id={props.id}
      onChange={(event) => {
        props.onChange(event.currentTarget.checked);
      }}
      type="checkbox"
    />
  );
}

export function Select<T extends string>(props: {
  readonly id?: string;
  readonly value: T;
  readonly options: readonly { readonly value: T; readonly label: string; readonly title?: string }[];
  readonly onChange: (value: T) => void;
  readonly ariaLabel?: string;
  readonly disabled?: boolean;
  readonly className?: string;
  /** Controlled: shows `value` on every render. */
  readonly controlled?: boolean;
  /** `data-*` attributes for the `<select>` itself, which Playwright selects on. */
  readonly data?: Readonly<Record<`data-${string}`, string>>;
}) {
  const common = {
    ...props.data,
    "aria-label": props.ariaLabel,
    className: cn(INPUT_INNER, "cursor-pointer appearance-none pe-8"),
    disabled: props.disabled,
    id: props.id,
    onChange: (event: { currentTarget: HTMLSelectElement }) => {
      props.onChange(event.currentTarget.value as T);
    },
  };
  return (
    <span className={cn(INPUT_SHELL, props.className)}>
      <select {...common} {...(props.controlled ? { value: props.value } : { defaultValue: props.value })}>
        {props.options.map((option) => (
          <option key={option.value} title={option.title} value={option.value}>
            {option.label}
          </option>
        ))}
      </select>
      <ChevronDownIcon
        aria-hidden="true"
        className="pointer-events-none absolute end-2.5 top-1/2 size-4 -translate-y-1/2 text-muted-foreground"
      />
    </span>
  );
}

export function Action(props: {
  readonly children: ReactNode;
  readonly onClick: () => unknown;
  readonly disabled?: boolean;
  readonly variant?: "default" | "outline" | "secondary" | "ghost";
  readonly title?: string;
  readonly "aria-label"?: string;
  readonly [data: `data-${string}`]: string | undefined;
}) {
  const { children, onClick, variant, ...rest } = props;
  return (
    <Button
      {...rest}
      onClick={() => {
        // Dropped on purpose: the action reports its own refusal on the card's error line.
        void onClick();
      }}
      size="sm"
      variant={variant ?? "outline"}
    >
      {children}
    </Button>
  );
}

export function Actions(props: { readonly children: ReactNode }) {
  return <div className="card-actions flex flex-wrap gap-2">{props.children}</div>;
}

export function ErrorLine(props: { readonly error: string | null }) {
  return (
    <p
      className="card-error rounded-md border border-destructive/24 bg-destructive/6 px-2.5 py-1.5 text-destructive-foreground text-xs"
      hidden={props.error === null}
      role="alert"
    >
      {props.error ?? ""}
    </p>
  );
}

export function Note(props: { readonly children: ReactNode }) {
  return <p className="card-note text-muted-foreground text-xs leading-relaxed">{props.children}</p>;
}

export function Readout(props: { readonly children: ReactNode; readonly [data: `data-${string}`]: string | undefined }) {
  const { children, ...rest } = props;
  return (
    <span className="readout font-mono text-foreground/80 text-xs tabular-nums" {...rest}>
      {children}
    </span>
  );
}
