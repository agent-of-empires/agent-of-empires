import { useCallback, useEffect, useRef, useState } from "react";
import { createProfile, deleteProfile, fetchProfiles, renameProfile } from "../../lib/api";
import type { ProfileInfo } from "../../lib/types";
import { validateProfileName } from "../profiles/profileName";

interface Props {
  selectedProfile: string;
  onSelect: (profile: string) => void;
}

export function ProfileSelector({ selectedProfile, onSelect }: Props) {
  const [profiles, setProfiles] = useState<ProfileInfo[]>([]);
  const [mode, setMode] = useState<"create" | "rename" | null>(null);
  const [inputValue, setInputValue] = useState("");
  const [error, setError] = useState<string | null>(null);
  const panelRef = useRef<HTMLDivElement>(null);

  const load = useCallback(() => {
    fetchProfiles().then(setProfiles);
  }, []);

  useEffect(() => {
    load();
  }, [load]);

  const activeProfile = profiles.find((p) => p.is_default);

  const openInput = (next: "create" | "rename" | null, value = "") => {
    setMode(next);
    setInputValue(value);
    setError(null);
  };
  const closeInput = () => openInput(null);

  useEffect(() => {
    if (!mode) return;
    const handler = (e: MouseEvent) => {
      if (panelRef.current && !panelRef.current.contains(e.target as Node)) {
        setMode(null);
        setInputValue("");
        setError(null);
      }
    };
    document.addEventListener("mousedown", handler);
    return () => document.removeEventListener("mousedown", handler);
  }, [mode]);

  const submitInput = async () => {
    if (!mode) return;
    const trimmed = inputValue.trim();
    if (mode === "rename" && trimmed === selectedProfile) return closeInput();
    const err = validateProfileName(trimmed);
    if (err) return setError(err);
    if (mode === "create" ? await createProfile(trimmed) : await renameProfile(selectedProfile, trimmed)) {
      if (mode === "rename") onSelect(trimmed);
      closeInput();
      load();
    } else {
      setError(`Failed to ${mode} profile`);
    }
  };

  const handleDelete = async (name: string) => {
    if (!confirm(`Delete profile "${name}"?`)) return;
    if (!(await deleteProfile(name))) return;
    const fallback = activeProfile?.name ?? "default";
    if (selectedProfile === name) onSelect(fallback === name ? "default" : fallback);
    load();
  };

  return (
    <div className="relative" ref={panelRef}>
      <div className="flex items-center gap-2 flex-nowrap">
        <label className="text-sm font-medium text-text-secondary shrink-0">Profile</label>
        <select
          value={selectedProfile}
          onChange={(e) => onSelect(e.target.value)}
          className="bg-surface-900 border border-surface-700 rounded-md px-2 py-1 text-sm text-text-primary focus:border-brand-600 focus:outline-none w-32 sm:w-40 shrink"
        >
          {profiles.map((p) => (
            <option key={p.name} value={p.name}>
              {p.name}
            </option>
          ))}
        </select>
        <button
          onClick={() => openInput("create")}
          className="text-sm text-brand-500 hover:text-brand-400 cursor-pointer shrink-0 font-medium px-1.5"
          title="Create new profile"
        >
          + New
        </button>
        {!mode && (
          <>
            <button
              onClick={() => openInput("rename", selectedProfile)}
              className="text-xs text-text-dim hover:text-text-primary cursor-pointer"
              title="Rename profile"
            >
              Rename
            </button>
            {(!activeProfile || activeProfile.name !== selectedProfile) && (
              <button
                onClick={() => handleDelete(selectedProfile)}
                className="text-xs text-text-dim hover:text-red-400 cursor-pointer"
                title="Delete profile"
              >
                Delete
              </button>
            )}
          </>
        )}
      </div>

      {mode && (
        <div className="absolute right-0 top-full mt-1 z-10 bg-surface-850 border border-surface-700 rounded-lg p-3 shadow-lg min-w-[280px]">
          <div className="flex gap-2">
            <input
              type="text"
              value={inputValue}
              onChange={(e) => {
                setInputValue(e.target.value);
                setError(null);
              }}
              onKeyDown={(e) => {
                if (e.key === "Enter") submitInput();
                if (e.key === "Escape") closeInput();
              }}
              placeholder={mode === "create" ? "Profile name" : "New name"}
              autoFocus
              className={`flex-1 bg-surface-900 border rounded-md px-2 py-1.5 text-sm text-text-primary focus:outline-none ${error ? "border-red-500" : "border-surface-700 focus:border-brand-600"}`}
            />
            <button
              onClick={submitInput}
              className="px-3 py-1.5 rounded-md bg-brand-600 hover:bg-brand-500 text-xs font-medium text-surface-950 cursor-pointer"
            >
              {mode === "create" ? "Create" : "Rename"}
            </button>
          </div>
          {error && <div className="text-xs text-red-400 mt-1">{error}</div>}
        </div>
      )}
    </div>
  );
}
