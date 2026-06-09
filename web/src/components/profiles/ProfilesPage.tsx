import { useEffect, useRef, useState } from "react";
import { useNavigate } from "react-router-dom";
import {
  createProfile,
  deleteProfile,
  fetchProfiles,
  fetchSettings,
  getProfileSettings,
  renameProfile,
  setDefaultProfile,
  updateProfileSettings,
} from "../../lib/api";
import type { HooksOverride, ProfileInfo, ProfileSettingsResponse } from "../../lib/types";
import { buildEffectiveHooks } from "../../lib/profileHooks";
import { HooksReadOnlyPanel } from "./HooksReadOnlyPanel";

interface Props {
  onClose: () => void;
  readOnly?: boolean;
}

// Sections deep-linked into the existing Settings page (scoped to the
// selected profile via ?profile=). Per-section editing, including the
// passphrase-gated sandbox/worktree saves, stays in SettingsView; this page
// owns profile-management metadata only.
const EDIT_SECTIONS: ReadonlyArray<{ tab: string; label: string }> = [
  { tab: "session", label: "Session" },
  { tab: "theme", label: "Theme" },
  { tab: "sandbox", label: "Sandbox" },
  { tab: "worktree", label: "Worktree" },
];

function validateName(name: string): string | null {
  if (!name) return "Name is required";
  if (!/^[a-zA-Z0-9_-]+$/.test(name)) return "Only letters, digits, hyphens, and underscores";
  return null;
}

export function ProfilesPage({ onClose, readOnly }: Props) {
  const navigate = useNavigate();
  const [profiles, setProfiles] = useState<ProfileInfo[]>([]);
  const [selected, setSelected] = useState<string>("");
  const [profileSettings, setProfileSettings] = useState<ProfileSettingsResponse | null>(null);
  const [globalHooks, setGlobalHooks] = useState<HooksOverride | undefined>(undefined);
  const [description, setDescription] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [creating, setCreating] = useState(false);
  const [renaming, setRenaming] = useState(false);
  const [inputValue, setInputValue] = useState("");

  // A request id guards against a slow response for a previously-selected
  // profile winning after a fast switch.
  const loadSeq = useRef(0);
  // Set once the user edits the description, so the async load's late
  // `setDescription` can't clobber an in-progress edit.
  const descriptionDirty = useRef(false);

  const loadProfileSettings = (name: string) => {
    const seq = ++loadSeq.current;
    descriptionDirty.current = false;
    if (!name) {
      setProfileSettings(null);
      setGlobalHooks(undefined);
      setDescription("");
      setError(null);
      return;
    }
    Promise.all([getProfileSettings(name), fetchSettings()])
      .then(([profile, global]) => {
        if (seq !== loadSeq.current) return;
        setProfileSettings(profile);
        setGlobalHooks(global?.hooks as HooksOverride | undefined);
        setError(null);
        if (!descriptionDirty.current) {
          setDescription(typeof profile?.description === "string" ? profile.description : "");
        }
      })
      .catch(() => {
        if (seq !== loadSeq.current) return;
        setProfileSettings(null);
        setGlobalHooks(undefined);
        setDescription("");
        setError("Failed to load profile settings");
      });
  };

  const reload = async () => {
    const list = await fetchProfiles();
    setProfiles(list);
  };

  useEffect(() => {
    let cancelled = false;
    (async () => {
      const list = await fetchProfiles();
      if (cancelled) return;
      setProfiles(list);
      const current = list.find((p) => p.is_default)?.name ?? list[0]?.name ?? "";
      setSelected(current);
      loadProfileSettings(current);
    })();
    return () => {
      cancelled = true;
    };
  }, []);

  const closeInput = () => {
    setCreating(false);
    setRenaming(false);
    setInputValue("");
    setError(null);
  };

  const handleCreate = async () => {
    const trimmed = inputValue.trim();
    const err = validateName(trimmed);
    if (err) {
      setError(err);
      return;
    }
    const ok = await createProfile(trimmed);
    if (!ok) {
      setError("Failed to create profile");
      return;
    }
    closeInput();
    setSelected(trimmed);
    loadProfileSettings(trimmed);
    await reload();
  };

  const handleRename = async () => {
    const trimmed = inputValue.trim();
    if (trimmed === selected) {
      closeInput();
      return;
    }
    const err = validateName(trimmed);
    if (err) {
      setError(err);
      return;
    }
    const ok = await renameProfile(selected, trimmed);
    if (!ok) {
      setError("Failed to rename profile");
      return;
    }
    closeInput();
    setSelected(trimmed);
    loadProfileSettings(trimmed);
    await reload();
  };

  const handleDelete = async (name: string) => {
    if (!confirm(`Delete profile "${name}"?`)) return;
    const ok = await deleteProfile(name);
    if (!ok) {
      setError("Failed to delete profile");
      return;
    }
    if (selected === name) {
      setSelected("");
      loadProfileSettings("");
    }
    await reload();
  };

  const handleSetDefault = async (name: string) => {
    const ok = await setDefaultProfile(name);
    if (ok) await reload();
  };

  const handleSaveDescription = async () => {
    setError(null);
    const trimmed = description.trim();
    const ok = await updateProfileSettings(selected, {
      description: trimmed ? trimmed : null,
    });
    if (!ok) {
      setError("Failed to save description");
      return;
    }
    descriptionDirty.current = false;
    await reload();
  };

  const selectedInfo = profiles.find((p) => p.name === selected);
  const isDefault = selectedInfo?.is_default ?? false;
  const hookGroups = buildEffectiveHooks(profileSettings?.hooks, globalHooks);

  return (
    <div className="flex flex-col h-full bg-surface-900">
      <div className="flex items-center justify-between px-4 py-3 border-b border-surface-700/30">
        <div>
          <h1 className="text-lg font-semibold text-text-primary">Profiles</h1>
          <p className="text-xs text-text-dim">Manage configuration profiles and inspect their lifecycle hooks.</p>
        </div>
        <div className="flex gap-2">
          {!readOnly && !creating && !renaming && (
            <button
              type="button"
              onClick={() => {
                setCreating(true);
                setInputValue("");
                setError(null);
              }}
              className="px-3 py-1.5 text-sm bg-brand-600 hover:bg-brand-700 text-surface-900 rounded-md cursor-pointer font-medium"
            >
              + New profile
            </button>
          )}
          <button
            type="button"
            onClick={onClose}
            className="px-3 py-1.5 text-sm border border-surface-700 text-text-secondary hover:bg-surface-800 rounded-md cursor-pointer"
          >
            Close
          </button>
        </div>
      </div>

      {error && (
        <div className="mx-4 mt-3 px-3 py-2 bg-red-900/20 border border-red-700/30 rounded-md">
          <p className="text-sm text-red-400">{error}</p>
        </div>
      )}

      {(creating || renaming) && (
        <div className="px-4 pt-3">
          <div className="flex gap-2 bg-surface-850 border border-surface-700 rounded-lg p-3">
            <input
              type="text"
              value={inputValue}
              autoFocus
              onChange={(e) => {
                setInputValue(e.target.value);
                setError(null);
              }}
              onKeyDown={(e) => {
                if (e.key === "Enter") {
                  if (creating) handleCreate();
                  else handleRename();
                }
                if (e.key === "Escape") closeInput();
              }}
              placeholder={creating ? "Profile name" : "New name"}
              className="flex-1 bg-surface-900 border border-surface-700 rounded-md px-2 py-1.5 text-sm text-text-primary focus:border-brand-600 focus:outline-none"
            />
            <button
              type="button"
              onClick={creating ? handleCreate : handleRename}
              className="px-3 py-1.5 rounded-md bg-brand-600 hover:bg-brand-500 text-xs font-medium text-surface-950 cursor-pointer"
            >
              {creating ? "Create" : "Rename"}
            </button>
            <button
              type="button"
              onClick={closeInput}
              className="px-3 py-1.5 rounded-md border border-surface-700 text-xs text-text-secondary hover:bg-surface-800 cursor-pointer"
            >
              Cancel
            </button>
          </div>
        </div>
      )}

      <div className="flex flex-1 min-h-0 gap-4 p-4">
        <nav className="w-56 shrink-0 flex flex-col gap-1 overflow-y-auto">
          {profiles.map((p) => (
            <button
              key={p.name}
              type="button"
              onClick={() => {
                setSelected(p.name);
                loadProfileSettings(p.name);
              }}
              className={`flex items-center justify-between rounded-md px-3 py-2 text-sm text-left cursor-pointer ${
                p.name === selected ? "bg-surface-700 text-text-primary" : "text-text-secondary hover:bg-surface-800"
              }`}
            >
              <span className="truncate">{p.name}</span>
              {p.is_default && (
                <span className="ml-2 shrink-0 rounded-md bg-brand-600/15 px-1.5 py-0.5 text-[11px] font-medium text-brand-400">
                  default
                </span>
              )}
            </button>
          ))}
        </nav>

        <div className="flex-1 min-w-0 overflow-y-auto">
          {selectedInfo ? (
            <div className="flex flex-col gap-4">
              <div className="flex items-center gap-2">
                <h2 className="text-base font-semibold text-text-primary">{selectedInfo.name}</h2>
                {!readOnly && (
                  <>
                    {!isDefault && (
                      <button
                        type="button"
                        onClick={() => handleSetDefault(selectedInfo.name)}
                        className="text-xs text-text-dim hover:text-text-primary cursor-pointer"
                      >
                        Set as default
                      </button>
                    )}
                    <button
                      type="button"
                      onClick={() => {
                        setRenaming(true);
                        setCreating(false);
                        setInputValue(selectedInfo.name);
                        setError(null);
                      }}
                      className="text-xs text-text-dim hover:text-text-primary cursor-pointer"
                    >
                      Rename
                    </button>
                    {!isDefault && (
                      <button
                        type="button"
                        onClick={() => handleDelete(selectedInfo.name)}
                        className="text-xs text-text-dim hover:text-red-400 cursor-pointer"
                      >
                        Delete
                      </button>
                    )}
                  </>
                )}
              </div>

              <div>
                <label className="block text-xs text-text-dim mb-1">Description</label>
                <div className="flex gap-2">
                  <input
                    type="text"
                    value={description}
                    disabled={readOnly}
                    onChange={(e) => {
                      descriptionDirty.current = true;
                      setDescription(e.target.value);
                    }}
                    placeholder="What this profile is for"
                    className="flex-1 bg-surface-900 border border-surface-700 rounded-md px-2 py-1.5 text-sm text-text-primary focus:border-brand-600 focus:outline-none disabled:opacity-60"
                  />
                  {!readOnly && (
                    <button
                      type="button"
                      onClick={handleSaveDescription}
                      className="px-3 py-1.5 rounded-md border border-surface-700 text-xs text-text-secondary hover:bg-surface-800 cursor-pointer"
                    >
                      Save
                    </button>
                  )}
                </div>
              </div>

              <div>
                <h3 className="text-sm font-semibold text-text-primary mb-2">Edit configuration</h3>
                <div className="flex flex-wrap gap-2">
                  {EDIT_SECTIONS.map((s) => (
                    <button
                      key={s.tab}
                      type="button"
                      onClick={() => navigate(`/settings/${s.tab}?profile=${encodeURIComponent(selectedInfo.name)}`)}
                      className="px-3 py-1.5 rounded-md border border-surface-700 text-xs text-text-secondary hover:bg-surface-800 cursor-pointer"
                    >
                      {s.label} &rarr;
                    </button>
                  ))}
                </div>
              </div>

              <HooksReadOnlyPanel groups={hookGroups} />
            </div>
          ) : (
            <p className="text-sm text-text-dim">
              {profiles.length === 0 ? "No profiles yet." : "Select a profile to view its details."}
            </p>
          )}
        </div>
      </div>
    </div>
  );
}
