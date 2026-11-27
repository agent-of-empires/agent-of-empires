import { useCallback, useEffect, useRef, useState } from "react";
import { createProfile, deleteProfile, fetchProfiles, renameProfile } from "../../lib/api";
import type { ProfileInfo } from "../../lib/types";
import { validateProfileName } from "../profiles/profileName";
import { isImeComposing } from "../../lib/ime";
import { INPUT, MENU } from "../../lib/surfaces";
import { BUTTON_DANGER_GHOST_SM, BUTTON_GHOST_SM, BUTTON_PRIMARY_SM } from "../../lib/surfaces";

interface Props {
  selectedProfile: string;
  onSelect: (profile: string) => void;
  onRenamed: () => Promise<void> | void;
}

export function ProfileSelector({ selectedProfile, onSelect, onRenamed }: Props) {
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
      if (mode === "rename") {
        const refreshed = onRenamed();
        onSelect(trimmed);
        await refreshed;
      }
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
        <label htmlFor="settings-profile-select" className="text-[15px] md:text-sm text-text-primary shrink-0">
          Editing profile
        </label>
        <select
          id="settings-profile-select"
          value={selectedProfile}
          onChange={(e) => onSelect(e.target.value)}
          className={`${INPUT} !w-32 sm:!w-40 shrink`}
        >
          {profiles.map((p) => (
            <option key={p.name} value={p.name}>
              {p.name}
            </option>
          ))}
        </select>
        <button
          onClick={() => openInput("create")}
          className={`${BUTTON_GHOST_SM} shrink-0`}
          title="Create new profile"
        >
          + New
        </button>
        {!mode && (
          <>
            <button
              onClick={() => openInput("rename", selectedProfile)}
              className={BUTTON_GHOST_SM}
              title="Rename profile"
            >
              Rename
            </button>
            {(!activeProfile || activeProfile.name !== selectedProfile) && (
              <button
                onClick={() => handleDelete(selectedProfile)}
                className={BUTTON_DANGER_GHOST_SM}
                title="Delete profile"
              >
                Delete
              </button>
            )}
          </>
        )}
      </div>

      {mode && (
        <div className={`absolute right-0 top-full mt-1 z-10 ${MENU.replace("py-1", "p-3")} min-w-[280px]`}>
          <div className="flex gap-2">
            <input
              type="text"
              value={inputValue}
              onChange={(e) => {
                setInputValue(e.target.value);
                setError(null);
              }}
              onKeyDown={(e) => {
                if (e.key === "Enter" && !isImeComposing(e)) submitInput();
                if (e.key === "Escape") closeInput();
              }}
              placeholder={mode === "create" ? "Profile name" : "New name"}
              autoFocus
              className={`${INPUT} !w-auto flex-1 ${error ? "!border-status-error" : ""}`}
            />
            <button onClick={submitInput} className={BUTTON_PRIMARY_SM}>
              {mode === "create" ? "Create" : "Rename"}
            </button>
          </div>
          {error && <div className="text-[13px] text-status-error-text mt-1">{error}</div>}
        </div>
      )}
    </div>
  );
}
