use super::*;
use ratatui::backend::TestBackend;
use ratatui::Terminal;

/// Seed the duplicate state against an already-isolated app dir: one session id
/// in both `alpha` and `beta`. Returns that id.
fn seed_ambiguous_profiles() -> String {
    let alpha = Storage::new_unwatched("alpha").unwrap();
    let mut inst = Instance::new("moved", "/repo/moved");
    inst.group_path = "work".to_string();
    let id = inst.id.clone();
    alpha
        .update(|i, g| {
            i.push(inst.clone());
            g.push(Group::new("work", "work"));
            Ok(())
        })
        .unwrap();
    let beta = Storage::new_unwatched("beta").unwrap();
    beta.update(|i, _| {
        let mut copy = inst.clone();
        copy.source_profile = "beta".to_string();
        i.push(copy);
        Ok(())
    })
    .unwrap();
    id
}

/// One session id present in `alpha` and `beta`, plus an optional valid
/// move journal claiming alpha -> beta (target published, target wins).
fn boot_ambiguous_state(with_journal: bool) -> (TempDir, AppDirGuard, String) {
    let temp = TempDir::new().unwrap();
    let guard = setup_test_home(&temp);
    let id = seed_ambiguous_profiles();
    if with_journal {
        arm_interrupted_move_journal(
            &Storage::new_unwatched("alpha").unwrap(),
            &Storage::new_unwatched("beta").unwrap(),
            &id,
        );
    }
    (temp, guard, id)
}

/// Record the same valid, unconsumed `alpha -> beta` move journal
/// [`boot_ambiguous_state`] writes, so a caller can arm it *after* the view has
/// already loaded the duplicates.
fn arm_interrupted_move_journal(alpha: &Storage, beta: &Storage, id: &str) {
    crate::session::record_move_journal(
        &crate::session::MoveJournalEntry {
            version: crate::session::MOVE_JOURNAL_VERSION,
            ids: vec![id.to_string()],
            source_profile: "alpha".to_string(),
            target_profile: "beta".to_string(),
            source_sessions_path: alpha.sessions_path().to_path_buf(),
            target_sessions_path: beta.sessions_path().to_path_buf(),
            group_move_source_path: "work".to_string(),
            group_move_target_path: "moved".to_string(),
            group_move_subtree: false,
            created_at_epoch_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or_default(),
        },
        alpha.sessions_path(),
    )
    .unwrap();
}

#[test]
#[serial]
fn interrupted_move_with_journal_repairs_before_publish() {
    let (_temp, _guard, id) = boot_ambiguous_state(true);

    let view = HomeView::new_for_test(
        None,
        AvailableTools::with_tools(&["claude"]),
        crate::file_watch::FileWatchService::noop(),
    )
    .unwrap();

    // The journal arbitrates before the unified map is published, so the
    // repaired row is usable immediately instead of being excluded.
    assert_eq!(view.instances.len(), 1, "exactly the winning row publishes");
    let row = view.instances.get(&id).expect("repaired row present");
    assert_eq!(row.source_profile, "beta", "target copy wins per journal");
    assert!(view.legacy_duplicate_reports.is_empty());
    assert!(
        Storage::new_unwatched("alpha")
            .unwrap()
            .load()
            .unwrap()
            .is_empty(),
        "losing source copy removed on disk"
    );
}

#[test]
#[serial]
fn legacy_duplicate_stays_excluded_and_is_surfaced() {
    let (_temp, _guard, id) = boot_ambiguous_state(false);

    let mut view = HomeView::new_for_test(
        None,
        AvailableTools::with_tools(&["claude"]),
        crate::file_watch::FileWatchService::noop(),
    )
    .unwrap();

    assert!(
        view.instances.get(&id).is_none(),
        "without journal evidence every copy stays excluded"
    );
    assert_eq!(view.legacy_duplicate_reports.len(), 1);
    let message = view.legacy_duplicate_reports[0].actionable_message();
    assert!(message.contains(&id) && message.contains("alpha") && message.contains("beta"));

    // The fail-closed state must be visible, not silent.
    let theme = crate::tui::styles::load_theme("empire");
    let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
    terminal
        .draw(|f| {
            let area = f.area();
            view.render(f, area, &theme, None, None, None);
        })
        .unwrap();
    let buf = terminal.backend().buffer();
    let mut out = String::new();
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            out.push_str(buf[(x, y)].symbol());
        }
        out.push('\n');
    }
    assert!(
        out.contains("\u{26a0} 1 ambiguous"),
        "the list title must flag ambiguous sessions.\nFull buffer:\n{out}"
    );
}

/// `create_session` publishes the new row and then reloads, and that reload
/// reconciles cross-profile duplicates. A journal-guided repair takes the
/// app-wide identity flock, so the publication path must release the flocks it
/// holds before reloading: keeping one across the reload is a self-deadlock,
/// because `flock` binds to the open file description and a second open of the
/// same file in the same process still blocks.
///
/// The `create_session` call itself runs on a worker thread, because the
/// regression is an unbounded self-deadlock: the main thread watches the
/// contention seam instead of waiting on a call that would never return. The
/// app-dir isolation stays on the main thread so it is released even when the
/// worker is still parked in the flock wait.
#[test]
#[serial]
fn create_session_publishes_before_reloading_with_the_ownership_flocks_released() {
    use std::sync::mpsc::{self, TryRecvError};

    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);
    let id = seed_ambiguous_profiles();
    let project_dir = temp.path().join("project");
    std::fs::create_dir_all(&project_dir).unwrap();

    let (contended_tx, contended_rx) = mpsc::channel::<std::path::PathBuf>();
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let worker = std::thread::spawn(move || {
        let mut view = HomeView::new_for_test(
            None,
            AvailableTools::with_tools(&["claude"]),
            crate::file_watch::FileWatchService::noop(),
        )
        .unwrap();
        // Armed after the view loaded, so the duplicates are still on disk and
        // the journal is still unconsumed when the publication reload runs.
        arm_interrupted_move_journal(
            &Storage::new_unwatched("alpha").unwrap(),
            &Storage::new_unwatched("beta").unwrap(),
            &id,
        );

        let mut data = creation_data(&project_dir, "Fresh Publish", "work");
        data.profile = "alpha".to_string();

        let _observer = crate::session::observe_lock_contention_for_test(contended_tx);
        let created = view
            .create_session(data)
            .expect("create_session must publish the new row");
        assert!(
            view.get_instance(&created).is_some(),
            "the new row must be published"
        );
        let _ = done_tx.send(());
    });

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        if let Ok(path) = contended_rx.try_recv() {
            // The seam reports the contended path on the very first WouldBlock,
            // which is the regression itself; fail here rather than waiting out
            // the deadline for a call that will never return.
            panic!("the publication path re-entered a flock it already held: {path:?}");
        }
        match done_rx.try_recv() {
            Ok(()) | Err(TryRecvError::Disconnected) => break,
            Err(TryRecvError::Empty) => {}
        }
        assert!(
            std::time::Instant::now() < deadline,
            "create_session never returned; the ownership flocks it held are still \
             blocking the publication reload"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }

    worker.join().unwrap();
}
