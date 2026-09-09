use super::*;
use ratatui::backend::TestBackend;
use ratatui::Terminal;

/// One session id present in `alpha` and `beta`, plus an optional valid
/// move journal claiming alpha -> beta (target published, target wins).
fn boot_ambiguous_state(with_journal: bool) -> (TempDir, AppDirGuard, String) {
    let temp = TempDir::new().unwrap();
    let guard = setup_test_home(&temp);
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
    if with_journal {
        crate::session::record_move_journal(
            &crate::session::MoveJournalEntry {
                version: crate::session::MOVE_JOURNAL_VERSION,
                ids: vec![id.clone()],
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
    (temp, guard, id)
}

#[test]
#[serial]
fn interrupted_move_with_journal_repairs_before_publish() {
    let (_temp, _guard, id) = boot_ambiguous_state(true);

    let view = HomeView::new(
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

    let mut view = HomeView::new(
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
