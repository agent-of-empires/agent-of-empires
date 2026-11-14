//! Register `WorkerRecord.launch_executable` and `launch_resolved_executable`.
//!
//! A running worker's spawn path cannot be recovered, so existing records stay unset until their
//! next spawn and are never offered an agent-update restart before then.

use anyhow::Result;

pub fn run() -> Result<()> {
    tracing::info!(
        "v057: worker launch executable is recorded on the next spawn; existing records stay unset"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::process::worker_registry::WorkerRecord;

    #[test]
    fn record_written_before_v057_loads_with_no_launch_executable() {
        let record = WorkerRecord::new(
            "t".into(),
            1,
            "/tmp/s".into(),
            "claude".into(),
            "claude".into(),
            "/tmp".into(),
            None,
            vec![],
            vec![],
            None,
            None,
        );
        let mut value = serde_json::to_value(record).unwrap();
        let object = value.as_object_mut().unwrap();
        object.remove("launch_executable");
        object.remove("launch_resolved_executable");
        let loaded: WorkerRecord = serde_json::from_value(value).unwrap();
        assert_eq!(loaded.launch_executable, None);
        assert_eq!(loaded.launch_resolved_executable, None);
    }
}
