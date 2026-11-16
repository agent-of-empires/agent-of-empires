import { useId, type ReactNode } from "react";
import { Check, Copy, Lock } from "lucide-react";

import type { SettingsFieldDescriptor } from "../../lib/types";
import { settingKey, settingTags } from "../../lib/settingRow";
import { COMPACT_COPY_BUTTON_CLASS, useCopy } from "../acp/useCopy";
import type { RowLabel } from "./rowLabel";

/** Where the control sits: right of the text, or under it for wide editors. Phones put `inline` under it too. */
export type ControlPlacement = "toggle" | "inline" | "block";

const CONTROL_CLASS: Record<ControlPlacement, string> = {
  toggle: "col-start-2 row-start-1 row-span-3 pt-0.5",
  inline: "col-span-2 pt-1.5 sm:col-span-1 sm:col-start-2 sm:row-start-1 sm:row-span-3 sm:w-60 sm:pt-0",
  block: "col-span-2 pt-2",
};

// With a mouse the key shows on hover or focus. Its line keeps its height while hidden, so rows don't jump,
// and opacity keeps the copy button reachable by Tab.
const KEY_REVEAL =
  "sm:pointer-fine:opacity-0 sm:pointer-fine:group-hover:opacity-100 sm:pointer-fine:group-focus-within:opacity-100";

/** A schema setting: label, tags and lock, its plain line, its copyable key, and the control. */
export function SettingRow({
  descriptor: d,
  placement,
  hideKey = false,
  children,
}: {
  descriptor: SettingsFieldDescriptor;
  placement: ControlPlacement;
  /** The setup wizard leaves out config keys. */
  hideKey?: boolean;
  children: (row: RowLabel) => ReactNode;
}) {
  const id = useId();
  const row: RowLabel = { labelId: `${id}-label`, controlId: `${id}-control` };
  const key = settingKey(d);
  const [copied, copy] = useCopy();

  return (
    <div className="group grid grid-cols-[minmax(0,1fr)_auto] gap-x-4 gap-y-0.5 py-2.5 sm:pointer-fine:-mx-2 sm:pointer-fine:rounded-md sm:pointer-fine:px-2 sm:pointer-fine:hover:bg-surface-850">
      <label htmlFor={row.controlId} className="col-start-1 block min-w-0">
        <span className="flex flex-wrap items-center gap-x-1.5 gap-y-1">
          <span id={row.labelId} className="text-sm text-text-primary">
            {d.label}
          </span>
          {settingTags(d).map((tag) => (
            <span
              key={tag}
              className="whitespace-nowrap rounded border border-dashed border-surface-700 px-1 font-mono text-[11px] leading-4 text-text-dim"
            >
              {tag}
            </span>
          ))}
          {d.web_write.policy === "requires_elevation" && (
            <Lock role="img" aria-label="Asks for the passphrase" className="h-[13px] w-[13px] text-text-dim" />
          )}
        </span>
        {d.summary && <span className="mt-0.5 block text-[13px] leading-normal text-text-secondary">{d.summary}</span>}
      </label>
      {!hideKey && (
        <div className={`col-start-1 flex items-center gap-1.5 font-mono text-[11.5px] text-text-dim ${KEY_REVEAL}`}>
          <span className="truncate">{key}</span>
          <button
            type="button"
            aria-label="Copy config key"
            onClick={() => void copy(key)}
            className={COMPACT_COPY_BUTTON_CLASS}
          >
            {copied ? <Check className="h-3 w-3" aria-hidden /> : <Copy className="h-3 w-3" aria-hidden />}
          </button>
          <span aria-live="polite" className="sr-only">
            {copied ? "Copied" : ""}
          </span>
        </div>
      )}
      <div className={CONTROL_CLASS[placement]}>{children(row)}</div>
    </div>
  );
}
