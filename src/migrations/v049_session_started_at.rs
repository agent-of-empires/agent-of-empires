//! Register `Instance.started_at`.
//!
//! A running process's launch time cannot be recovered, so existing rows stay unset until their
//! next launch and are never offered an agent-update restart before then.

use anyhow::Result;

pub fn run() -> Result<()> {
    tracing::info!(
        "v049: session started_at is recorded on the next launch; existing rows stay unset"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::session::Instance;

    #[test]
    fn row_written_before_v049_loads_with_no_started_at() {
        let mut row = serde_json::to_value(Instance::new("t", "/tmp")).unwrap();
        row.as_object_mut().unwrap().remove("started_at");
        let loaded: Instance = serde_json::from_value(row).unwrap();
        assert_eq!(loaded.started_at, None);
    }
}
