use std::process::Command;

#[test]
fn omp_approval_overlay_rejects_symlinks_and_existing_paths() {
    let temp = tempfile::tempdir().unwrap();
    let script =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/capture-fixtures.sh");
    let target = temp.path().join("target.yml");
    let dangling_link = temp.path().join("dangling.yml");
    std::os::unix::fs::symlink(&target, &dangling_link).unwrap();

    let symlink_attempt = Command::new(&script)
        .args([
            "--write-omp-approval-overlay",
            dangling_link.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(!symlink_attempt.status.success());
    assert!(!target.exists());
    assert!(std::fs::symlink_metadata(&dangling_link).is_ok());

    let existing = temp.path().join("existing.yml");
    std::fs::write(&existing, "keep this file\n").unwrap();
    let existing_attempt = Command::new(&script)
        .args(["--write-omp-approval-overlay", existing.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(!existing_attempt.status.success());
    assert_eq!(
        std::fs::read_to_string(existing).unwrap(),
        "keep this file\n"
    );

    let fresh = temp.path().join("fresh.yml");
    let created = Command::new(&script)
        .args([
            "--write-omp-approval-overlay",
            fresh.to_str().unwrap(),
            "compact",
        ])
        .output()
        .unwrap();
    assert!(created.status.success());
    let config = std::fs::read_to_string(fresh).unwrap();
    assert!(config.contains("approvalMode: always-ask"));
    assert!(config.contains("bash: prompt"));
    assert!(config.contains("preset: compact"));
}
