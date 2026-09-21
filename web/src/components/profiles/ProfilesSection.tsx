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
import { validateProfileName } from "./profileName";

interface Props {
  readOnly?: boolean;
}

// Per-section editing stays in those Settings tabs, scoped via ?profile=.
const EDIT_SECTIONS: ReadonlyArray<{ tab: string; label: string }> = [
  { tab: "session", label: "Session" },
  { tab: "theme", label: "Theme" },
  { tab: "sandbox", label: "Sandbox" },
  { tab: "worktree", label: "Worktree" },
];

const SECONDARY_BUTTON =
  "px-3 py-1.5 rounded-md border border-surface-700 text-xs text-text-secondary hover:bg-surface-800 cursor-pointer";
const LINK_BUTTON = "text-xs text-text-dim hover:text-text-primary cursor-pointer";

export function ProfilesSection({ readOnly }: Props) {
  const navigate = useNavigate();
  const [profiles, setProfiles] = useState<ProfileInfo[]>([]);
  const [selected, setSelected] = useState<string>("");
  const [profileSettings, setProfileSettings] = useState<ProfileSettingsResponse | null>(null);
  const [globalHooks, setGlobalHooks] = useState<HooksOverride | undefined>(undefined);
  const [description, setDescription] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [nameInput, setNameInput] = useState<{ mode: "create" | "rename"; value: string } | null>(null);

  // Drops a slow load for a previously selected profile.
  const loadSeq = useRef(0);
  // A late load must not clobber an in-progress description edit.
  const descriptionDirty = useRef(false);

  const selectProfile = (name: string) => {
    setSelected(name);
    const seq = ++loadSeq.current;
    descriptionDirty.current = false;
    const clear = (err: string | null) => {
      setProfileSettings(null);
      setGlobalHooks(undefined);
      setDescription("");
      setError(err);
    };
    if (!name) return clear(null);
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
        if (seq === loadSeq.current) clear("Failed to load profile settings");
      });
  };

  const reload = async () => setProfiles(await fetchProfiles());

  useEffect(() => {
    let cancelled = false;
    (async () => {
      const list = await fetchProfiles();
      if (cancelled) return;
      setProfiles(list);
      selectProfile(list.find((p) => p.is_default)?.name ?? list[0]?.name ?? "");
    })();
    return () => {
      cancelled = true;
    };
  }, []);

  const openInput = (mode: "create" | "rename", value: string) => {
    setNameInput({ mode, value });
    setError(null);
  };

  const closeInput = () => {
    setNameInput(null);
    setError(null);
  };

  const submitName = async () => {
    if (!nameInput) return;
    const { mode } = nameInput;
    const trimmed = nameInput.value.trim();
    if (mode === "rename" && trimmed === selected) return closeInput();
    const err = validateProfileName(trimmed);
    if (err) return setError(err);
    const ok = mode === "create" ? await createProfile(trimmed) : await renameProfile(selected, trimmed);
    if (!ok) return setError(`Failed to ${mode} profile`);
    closeInput();
    selectProfile(trimmed);
    await reload();
  };

  const handleDelete = async (name: string) => {
    if (!confirm(`Delete profile "${name}"?`)) return;
    if (!(await deleteProfile(name))) return setError("Failed to delete profile");
    if (selected === name) selectProfile("");
    await reload();
  };

  const handleSetDefault = async (name: string) => {
    if (await setDefaultProfile(name)) await reload();
  };

  const handleSaveDescription = async () => {
    setError(null);
    const trimmed = description.trim();
    if (!(await updateProfileSettings(selected, { description: trimmed ? trimmed : null }))) {
      return setError("Failed to save description");
    }
    descriptionDirty.current = false;
    await reload();
  };

  const selectedInfo = profiles.find((p) => p.name === selected);
  const isDefault = selectedInfo?.is_default ?? false;
  const creating = nameInput?.mode === "create";

  return (
    <div className="space-y-3" data-testid="profiles-section">
      <p className="text-xs text-text-dim">Manage configuration profiles and inspect their lifecycle hooks.</p>

      {!readOnly && !nameInput && (
        <button
          type="button"
          onClick={() => openInput("create", "")}
          className="px-3 py-1.5 text-sm bg-brand-600 hover:bg-brand-700 text-surface-900 rounded-md cursor-pointer font-medium"
        >
          + New profile
        </button>
      )}

      {error && (
        <div className="px-3 py-2 bg-red-900/20 border border-red-700/30 rounded-md">
          <p className="text-sm text-red-400">{error}</p>
        </div>
      )}

      {nameInput && (
        <div className="flex gap-2 bg-surface-850 border border-surface-700 rounded-lg p-3">
          <input
            type="text"
            value={nameInput.value}
            autoFocus
            onChange={(e) => {
              setNameInput({ ...nameInput, value: e.target.value });
              setError(null);
            }}
            onKeyDown={(e) => {
              if (e.key === "Enter") submitName();
              if (e.key === "Escape") closeInput();
            }}
            placeholder={creating ? "Profile name" : "New name"}
            className="flex-1 bg-surface-900 border border-surface-700 rounded-md px-2 py-1.5 text-sm text-text-primary focus:border-brand-600 focus:outline-none"
          />
          <button
            type="button"
            onClick={submitName}
            className="px-3 py-1.5 rounded-md bg-brand-600 hover:bg-brand-500 text-xs font-medium text-surface-950 cursor-pointer"
          >
            {creating ? "Create" : "Rename"}
          </button>
          <button type="button" onClick={closeInput} className={SECONDARY_BUTTON}>
            Cancel
          </button>
        </div>
      )}

      <div className="flex gap-4">
        <nav className="w-44 shrink-0 flex flex-col gap-1">
          {profiles.map((p) => (
            <button
              key={p.name}
              type="button"
              onClick={() => selectProfile(p.name)}
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

        <div className="flex-1 min-w-0">
          {selectedInfo ? (
            <div className="flex flex-col gap-4">
              <div className="flex items-center gap-2">
                <h2 className="text-base font-semibold text-text-primary">{selectedInfo.name}</h2>
                {!readOnly && (
                  <>
                    {!isDefault && (
                      <button type="button" onClick={() => handleSetDefault(selectedInfo.name)} className={LINK_BUTTON}>
                        Set as default
                      </button>
                    )}
                    <button
                      type="button"
                      onClick={() => openInput("rename", selectedInfo.name)}
                      className={LINK_BUTTON}
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
                    <button type="button" onClick={handleSaveDescription} className={SECONDARY_BUTTON}>
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
                      className={SECONDARY_BUTTON}
                    >
                      {s.label} &rarr;
                    </button>
                  ))}
                </div>
              </div>

              <HooksReadOnlyPanel groups={buildEffectiveHooks(profileSettings?.hooks, globalHooks)} />
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
