use anyhow::{Context, Result};

pub fn run() -> Result<()> {
    let _transaction = crate::daemon::lifecycle::Transaction::acquire_blocking()?;
    let root = crate::session::AnchoredDir::open(&crate::session::get_app_dir()?)?;
    let current = root
        .bind_file(std::ffi::OsStr::new("serve.launch"))?
        .read()?
        .map(|raw| serde_json::from_str::<serde_json::Value>(&raw))
        .transpose()?;
    let passphrase = root
        .bind_file(std::ffi::OsStr::new("serve.passphrase"))?
        .read()?;
    for name in ["serve.launch", "serve.rollback.launch"] {
        let file = root.bind_file(std::ffi::OsStr::new(name))?;
        let Some(raw) = file.read()? else { continue };
        let mut launch: serde_json::Value = serde_json::from_str(&raw)?;
        let object = launch
            .as_object_mut()
            .context("Invalid daemon launch record")?;
        if object.contains_key("has_passphrase") {
            continue;
        }
        let current_identity = current.as_ref().is_some_and(|current| {
            current
                .get("pid")
                .is_some_and(|pid| pid.is_u64() && Some(pid) == object.get("pid"))
                && current
                    .get("instance_id")
                    .and_then(|value| value.as_str())
                    .is_some_and(|instance| {
                        !instance.is_empty()
                            && object.get("instance_id").and_then(|value| value.as_str())
                                == Some(instance)
                    })
        });
        let has_passphrase = if name == "serve.launch" || current_identity {
            passphrase.is_some()
                || object.get("remote").and_then(|value| value.as_bool()) == Some(true)
                || object.get("auth_mode").and_then(|value| value.as_str()) == Some("passphrase")
        } else {
            // An unrelated token launch may have had a secondary login gate.
            object.get("core_only").and_then(|value| value.as_bool()) != Some(true)
                && object.get("auth_mode").and_then(|value| value.as_str()) != Some("none")
        };
        object.insert("has_passphrase".into(), has_passphrase.into());
        object.insert("schema".into(), 4.into());
        if name == "serve.rollback.launch" && (!has_passphrase || current_identity) {
            let credential = serde_json::json!({
                "pid": object.get("pid"),
                "instance_id": object.get("instance_id"),
                "passphrase": if has_passphrase { passphrase.as_deref() } else { None },
            });
            root.bind_file(std::ffi::OsStr::new("serve.rollback.passphrase"))?
                .replace(&serde_json::to_vec(&credential)?)?;
        }
        file.replace(&serde_json::to_vec_pretty(&launch)?)?;
        tracing::info!(name, "Migrated persisted daemon passphrase policy");
    }
    Ok(())
}
