//! Register `Instance.launch_resolved_executable`.
//!
//! The resolved launch path cannot be recovered for a running process, so existing rows stay
//! unset until their next launch and are judged by target mtime alone.

use anyhow::Result;

pub fn run() -> Result<()> {
    tracing::info!(
        "v056: session launch_resolved_executable is recorded on the next launch; existing rows stay unset"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::session::Instance;

    #[test]
    fn row_written_before_v056_loads_with_no_resolved_executable() {
        let mut row = serde_json::to_value(Instance::new("t", "/tmp")).unwrap();
        row.as_object_mut()
            .unwrap()
            .remove("launch_resolved_executable");
        let loaded: Instance = serde_json::from_value(row).unwrap();
        assert_eq!(loaded.launch_resolved_executable, None);
    }
}
