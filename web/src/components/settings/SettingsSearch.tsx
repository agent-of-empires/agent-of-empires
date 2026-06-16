import { useMemo, useState } from "react";
import { Command } from "cmdk";
import type { SettingsFieldDescriptor } from "../../lib/types";
import { buildSettingsSearchIndex, type SettingsSearchHit } from "./settingsSearchIndex";

interface Props {
  schema: SettingsFieldDescriptor[];
  loading: boolean;
  onJump: (hit: SettingsSearchHit) => void;
}

// Full-text settings search. Sits in the settings header and filters the
// schema-backed settings as you type, mirroring the TUI `/` overlay: selecting
// a hit jumps to that field's tab (SettingsView scrolls it into view). cmdk
// provides the fuzzy filtering and arrow/Enter keyboard navigation; the index
// is built once from the cached schema.
export function SettingsSearch({ schema, loading, onJump }: Props) {
  const [query, setQuery] = useState("");
  const index = useMemo(() => buildSettingsSearchIndex(schema), [schema]);
  const open = query.trim().length > 0;

  const jump = (hit: SettingsSearchHit) => {
    onJump(hit);
    setQuery("");
  };

  return (
    <Command
      label="Search settings"
      data-testid="settings-search"
      className="relative"
      // Let the click on a result fire before the list unmounts on blur.
      onClick={(e) => e.stopPropagation()}
    >
      <Command.Input
        value={query}
        onValueChange={setQuery}
        placeholder={loading ? "Loading settings..." : "Search settings..."}
        disabled={loading}
        className="w-full rounded-md bg-surface-800 border border-surface-700 px-3 py-2 text-sm text-text-primary outline-none placeholder:text-text-muted focus:border-brand-500 disabled:opacity-50"
      />
      {open && (
        <Command.List className="absolute left-0 right-0 top-full mt-1 z-20 max-h-[40vh] overflow-y-auto rounded-md border border-surface-700 bg-surface-800 p-1 shadow-2xl">
          <Command.Empty className="px-3 py-4 text-center text-sm text-text-muted">No matching settings</Command.Empty>
          {index.map((hit) => (
            <Command.Item
              key={`${hit.section}.${hit.field}`}
              value={`${hit.section}.${hit.field} ${hit.searchText}`}
              onSelect={() => jump(hit)}
              data-testid={`settings-search-hit-${hit.section}-${hit.field}`}
              className="flex items-center gap-2 px-3 py-2 rounded-md cursor-pointer text-sm text-text-primary data-[selected=true]:bg-surface-700 data-[selected=true]:text-text-bright"
            >
              <span className="truncate">{hit.label}</span>
              <span className="flex-1" />
              <span className="shrink-0 text-[10px] font-mono uppercase tracking-wider text-text-muted">
                {hit.category}
              </span>
            </Command.Item>
          ))}
        </Command.List>
      )}
    </Command>
  );
}
