//! OMP loads user hooks as TypeScript modules from its agent directory.

use std::path::Path;

use anyhow::{bail, Context, Result};

use crate::agents::ResolvedHookEvent;

use super::command::hook_command;
use super::config_io::{log_installed, log_removed, log_unchanged, with_config_lock, write_file};
use super::HookInstallTarget;

/// Resolve only home/profile/environment OMP locations when no launch cwd exists.
pub(crate) fn context_free_agent_dir(
    home: &Path,
    host_environment: &[String],
    options: &crate::session::capture::OmpCliCaptureOptions,
) -> Option<std::path::PathBuf> {
    let mut environment = crate::session::capture::host_launcher_environment(host_environment);
    let resolved_home = environment
        .get("HOME")
        .filter(|value| !value.is_empty())
        .map(std::path::PathBuf::from)
        .filter(|home| home.is_absolute())
        .unwrap_or_else(|| home.to_path_buf());
    environment.insert("HOME".into(), resolved_home.to_str()?.to_owned());
    if environment
        .get("PI_CODING_AGENT_DIR")
        .is_some_and(|value| !Path::new(value).is_absolute())
    {
        environment.remove("PI_CODING_AGENT_DIR");
    }
    let cwd = resolved_home.to_str()?;
    crate::session::capture::resolve_omp_store_layout_with_environment(environment, cwd, options)
        .ok()
        .map(|context| context.agent_dir)
}
const MARKER: &str = "// aoe-hooks: managed OMP status module\n";

pub(crate) fn is_omp_hook(content: &str) -> bool {
    content.starts_with(MARKER)
}

pub fn install_omp_hooks_with_events(
    path: &Path,
    target: HookInstallTarget,
    events: &[ResolvedHookEvent],
) -> Result<()> {
    if events.iter().all(|event| event.status.is_none()) && !path.exists() {
        return Ok(());
    }
    with_config_lock(path, "lock", || {
        let existing = match std::fs::read_to_string(path) {
            Ok(content) => Some(content),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to read OMP hook {}", path.display()));
            }
        };
        if existing
            .as_deref()
            .is_some_and(|content| !is_omp_hook(content))
        {
            bail!("refusing to replace non-AoE OMP hook at {}", path.display());
        }
        let mut source = String::from(MARKER);
        source.push_str(
            "import { execFileSync } from \"node:child_process\";\n\
             export default function aoeStatus(pi) {\n\
               const write = (command) => {\n\
                 try { execFileSync(\"/bin/sh\", [\"-c\", command], { stdio: \"ignore\", timeout: 1000 }); } catch {}\n\
               };\n",
        );
        let mut installed = false;
        for event in events {
            let Some(status) = event.status else {
                continue;
            };
            installed = true;
            let event_name = serde_json::to_string(&event.name)?;
            let command = serde_json::to_string(&hook_command(status.as_str(), target))?;
            if event.name == "agent_end" {
                source.push_str(&format!(
                    "  pi.on({event_name}, (event, ctx) => {{ if (ctx.agent.kind === \"main\" && !event.willContinue) write({command}); }});\n"
                ));
            } else {
                source.push_str(&format!(
                    "  pi.on({event_name}, (_event, ctx) => {{ if (ctx.agent.kind === \"main\") write({command}); }});\n"
                ));
            }
        }
        if !installed {
            if existing.is_some() {
                std::fs::remove_file(path)?;
                log_removed(path);
            }
            return Ok(());
        }
        source.push_str("}\n");
        if existing.as_deref() == Some(source.as_str()) {
            log_unchanged(path);
            return Ok(());
        }
        write_file(path, source.as_bytes())?;
        log_installed(path);
        Ok(())
    })
}

pub fn uninstall_omp_hooks(path: &Path) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    with_config_lock(path, "lock", || {
        let content = std::fs::read_to_string(path)?;
        if !is_omp_hook(&content) {
            return Ok(false);
        }
        std::fs::remove_file(path)?;
        log_removed(path);
        Ok(true)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn managed_hook_install_preserves_foreign_modules_and_uninstalls_its_own() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("aoe-status.ts");
        let events = crate::agents::resolved_sidecar_hook_events(
            crate::agents::get_agent("omp").unwrap(),
            &crate::session::config::Config::default(),
        )
        .unwrap();

        install_omp_hooks_with_events(&path, HookInstallTarget::Host, &events).unwrap();
        assert!(is_omp_hook(&std::fs::read_to_string(&path).unwrap()));
        assert!(uninstall_omp_hooks(&path).unwrap());
        assert!(!path.exists());

        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let foreign = b"export default function userHook() {}\n";
        std::fs::write(&path, foreign).unwrap();
        assert!(install_omp_hooks_with_events(&path, HookInstallTarget::Host, &events).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), foreign);
        assert!(!uninstall_omp_hooks(&path).unwrap());

        let invalid_utf8 = b"\xff\xfe";
        std::fs::write(&path, invalid_utf8).unwrap();
        let error =
            install_omp_hooks_with_events(&path, HookInstallTarget::Host, &events).unwrap_err();
        assert!(error.to_string().contains("failed to read OMP hook"));
        assert_eq!(std::fs::read(&path).unwrap(), invalid_utf8);
    }
}
