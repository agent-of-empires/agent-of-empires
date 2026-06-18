# Hook status: manual recovery

AoE writes per-session status into `/tmp/aoe-hooks-<euid>/<id>/`. When init
fails, the TUI logs at `tracing::error!` with the resolved path and a
recovery hint, and AoE falls back to pane-detection (status latency goes
from ~1s to ~5s, no functional loss).

## Inspect the per-user base

```bash
ls -ldn /tmp/aoe-hooks-$(id -u)
# Expected: drwx------ 2 <your-uid> <your-gid> ... aoe-hooks-<your-uid>
```

## Recovery by symptom

| Symptom | Cause | Fix |
| --- | --- | --- |
| Owned by another uid | Another local user squatted the path. `/tmp` sticky bit prevents you from removing it. | Ask the squatter to remove it, or wait for them to log out, or have root run `rm -rf`. |
| Mode wrong (`drwxr-xr-x`, `drwxrwx---`, etc.) | Created with a permissive umask, or with setuid/setgid/sticky set. | `rm -rf /tmp/aoe-hooks-$(id -u) && aoe add`. |
| Symlink at the base path | Hostile pre-squat with a symlink. Init refused with `ELOOP`. | `rm -f /tmp/aoe-hooks-$(id -u)`. |
| Legacy `/tmp/aoe-hooks/` still present | Migration v016 ran but a per-target hook rewrite failed; the legacy directory was kept for inspection. | Run `aoe uninstall && aoe add` to fully reinstall hooks. After that, `rm -rf /tmp/aoe-hooks` removes the legacy directory if you own it. |
| ACL grants other uids access (Linux) | Operator or admin applied a `setfacl` that widens permissions. The shell snippet rejects writes; the Rust reader currently does not. | `setfacl -b /tmp/aoe-hooks-$(id -u)` clears extra ACL entries. |
| `/tmp` reaped the directory mid-run | systemd-tmpfiles or macOS `periodic.daily` deleted the base while AoE held a cached fd; reads see the orphan inode, writes land on the new path. | Restart `aoe`. Pane-detection covers the gap. |

## Disable hook-based detection entirely

```bash
aoe uninstall
```

Removes hooks from every known agent settings file and tears down
`/tmp/aoe-hooks-<euid>/`. AoE keeps detecting status via pane content.
