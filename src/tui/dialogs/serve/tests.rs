use super::*;
use std::sync::mpsc;

fn remove_app_file(name: &str) {
    if let Ok(app) = crate::session::get_app_dir() {
        let _ = std::fs::remove_file(app.join(name));
    }
}

fn key(code: char) -> KeyEvent {
    KeyEvent::from(KeyCode::Char(code))
}

fn picker_state() -> ServeViewState {
    ServeViewState::ModePicker {
        selected: ServeMode::Tunnel,
        tunnel_available: true,
        local_available: true,
        flash: None,
    }
}

fn confirm_state() -> ServeViewState {
    ServeViewState::Confirm {
        selected: TunnelTransport::Cloudflare,
        tailscale: TransportStatus::Ready,
        cloudflare: TransportStatus::Ready,
        flash: None,
    }
}

fn error_state() -> ServeViewState {
    ServeViewState::Error {
        message: "tailscale funnel failed".to_owned(),
        mode: ServeMode::Tunnel,
        transport: Some(TunnelTransport::Tailscale),
    }
}

fn active_state(mode: ServeMode) -> ServeViewState {
    ServeViewState::Active {
        mode,
        transport: (mode == ServeMode::Tunnel).then_some(TunnelTransport::Cloudflare),
        urls: vec![ServeUrl {
            label: None,
            url: match mode {
                ServeMode::Local => "http://127.0.0.1:8080/?token=t",
                ServeMode::Tunnel => "https://host.tailnet.ts.net/",
            }
            .to_owned(),
        }],
        url_index: 0,
        passphrase: (mode == ServeMode::Tunnel).then(|| "running daemon phrase".to_owned()),
        opened_at: Instant::now(),
        log_offset: 0,
    }
}

fn starting() -> ServeViewState {
    ServeViewState::Starting {
        mode: ServeMode::Local,
        transport: None,
        passphrase: None,
        started_at: Instant::now(),
        log_offset: 0,
    }
}

fn wait_for(view: &mut ServeView, predicate: impl Fn(&ServeView) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !predicate(view) {
        view.tick();
        assert!(Instant::now() < deadline, "Serve transition did not finish");
        std::thread::yield_now();
    }
}

fn wait_assessment(view: &mut ServeView) {
    wait_for(view, |view| {
        !view.pending.as_ref().is_some_and(|pending| {
            matches!(
                pending.kind,
                PendingJob::Restart { .. } | PendingJob::LaunchContext(_)
            )
        })
    });
}

fn snapshot() -> JobResult {
    JobResult::Snapshot(work::Snapshot {
        state: picker_state(),
        target: crate::cli::serve::tui_control_target().unwrap(),
        passphrase: Some("saved test passphrase".to_owned()),
        local_primary: Some("fresh interface".to_owned()),
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Held {
    Open,
    Picker,
    Providers,
    Retry,
    Stop,
    Reset,
    Restart,
    LaunchContext,
    LaunchOutcome,
    Starting,
    Passphrase,
    ErrorContext,
    Active,
    Copy,
}

fn operation(job: &Job) -> Held {
    match job {
        Job::Open => Held::Open,
        Job::Picker => Held::Picker,
        Job::Providers => Held::Providers,
        Job::Retry(_) => Held::Retry,
        Job::Stop(_) => Held::Stop,
        Job::Reset(_) => Held::Reset,
        Job::Restart(..) => Held::Restart,
        Job::LaunchContext { .. } => Held::LaunchContext,
        Job::LaunchOutcome { .. } => Held::LaunchOutcome,
        Job::ObserveStarting(_) => Held::Starting,
        Job::Passphrase => Held::Passphrase,
        Job::ErrorContext => Held::ErrorContext,
        Job::ObserveActive(_) => Held::Active,
        Job::Copy(..) => Held::Copy,
    }
}

struct Gate {
    entered: mpsc::Receiver<Held>,
    release: mpsc::Sender<JobResult>,
}

impl Gate {
    fn entered(&self, expected: Held) {
        assert_eq!(
            self.entered.recv_timeout(Duration::from_secs(10)).unwrap(),
            expected,
        );
    }

    fn release(&self, result: JobResult) {
        assert!(self.release.send(result).is_ok());
    }
}

fn gated(jobs: &mut ServeJobs, held: Held) -> (Submitter, Gate) {
    let (entered_tx, entered) = mpsc::channel();
    let (release, release_rx) = mpsc::channel();
    let submitter = jobs.with_handler(move |job, control, runtime| {
        let kind = operation(&job);
        if kind == held {
            entered_tx.send(kind).unwrap();
            return release_rx
                .recv_timeout(Duration::from_secs(10))
                .unwrap_or_else(|_| panic!("held operation was not released: {kind:?}"));
        }
        match job {
            Job::Open | Job::Picker => snapshot(),
            Job::ObserveActive(_) => JobResult::Active {
                log_offset: 0,
                passphrase: None,
            },
            _ => run_job(job, control, runtime),
        }
    });
    (submitter, Gate { entered, release })
}

#[test]
fn serve_mode_file_token_parses_server_output() {
    for mode in [ServeMode::Local, ServeMode::Tunnel] {
        assert_eq!(
            ServeMode::from_file_token(&format!("{}\n", mode.file_token())),
            Some(mode)
        );
    }
    for invalid in ["garbage", ""] {
        assert_eq!(ServeMode::from_file_token(invalid), None);
    }
}

#[test]
fn a_tunnel_retry_preselects_the_provider_that_did_not_fail() {
    use TransportStatus::{FunnelNotEnabled, NotInstalled, Ready};
    use TunnelTransport::{Cloudflare, Tailscale};
    for (failed, tailscale, cloudflare, expected) in [
        (Some(Tailscale), Ready, Ready, Cloudflare),
        (Some(Tailscale), Ready, NotInstalled, Tailscale),
        (Some(Cloudflare), Ready, Ready, Tailscale),
        (Some(Cloudflare), FunnelNotEnabled, Ready, Cloudflare),
        (None, Ready, Ready, Tailscale),
        (None, NotInstalled, Ready, Cloudflare),
    ] {
        assert!(matches!(
            ServeViewState::provider_retry(failed, tailscale, cloudflare),
            ServeViewState::Confirm { selected, .. } if selected == expected
        ));
    }
}

#[test]
fn a_local_error_offers_no_provider_retry_or_funnel_reset() {
    let mut view = ServeView::for_test(ServeViewState::Error {
        message: "tailscale status failed".to_owned(),
        mode: ServeMode::Local,
        transport: None,
    });
    for code in ['p', 'P', 'r', 'R'] {
        assert!(matches!(view.handle_key(key(code)), ServeAction::Continue));
        assert!(view.pending.is_none());
        assert!(matches!(
            view.state,
            ServeViewState::Error {
                mode: ServeMode::Local,
                ..
            }
        ));
    }
}

#[test]
fn shared_log_context_never_grants_funnel_reset() {
    for has_process_marker in [false, true] {
        for transport in [
            None,
            Some(TunnelTransport::Cloudflare),
            Some(TunnelTransport::Tailscale),
        ] {
            let known_funnel = transport == Some(TunnelTransport::Tailscale);
            let context = if known_funnel {
                "unrelated startup diagnostics"
            } else {
                "tailscale funnel failed in another process"
            };
            let mut view = ServeView::for_test(ServeViewState::start_failed_with(
                ServeMode::Tunnel,
                transport,
                "Dashboard start failed.",
                LogContext {
                    lines: vec![context.to_owned()],
                    has_process_marker,
                },
            ));
            for width in [60, 110] {
                let buffer = crate::tui::dialogs::test_render::draw(width, 40, |frame, theme| {
                    view.render(frame, frame.area(), theme)
                });
                let rendered: String = buffer.content.iter().map(|cell| cell.symbol()).collect();
                assert_eq!(
                    rendered.contains("[R]"),
                    known_funnel,
                    "transport={transport:?}; associated={has_process_marker}; width={width}"
                );
                for (label, code) in [
                    ("[P]", KeyCode::Char('P')),
                    ("[S]", KeyCode::Char('S')),
                    ("[Enter]", KeyCode::Enter),
                ] {
                    let (col, row) = crate::tui::dialogs::test_render::find(&buffer, label);
                    assert_eq!(
                        view.handle_click(col, row).map(|event| event.code),
                        Some(code)
                    );
                }
                if known_funnel {
                    let (col, row) = crate::tui::dialogs::test_render::find(&buffer, "[R]");
                    assert_eq!(
                        view.handle_click(col, row).map(|event| event.code),
                        Some(KeyCode::Char('R'))
                    );
                }
            }
            if !known_funnel {
                assert!(matches!(view.handle_key(key('R')), ServeAction::Continue));
                assert!(matches!(
                    view.handle_key(KeyEvent::from(KeyCode::Enter)),
                    ServeAction::Close
                ));
            }
        }
    }
}

#[test]
#[serial_test::serial]
fn r_g_and_help_reread_persist_sessions() {
    let _home = crate::session::test_support::isolate_app_dir();
    for code in ['R', 'G', '?'] {
        let mut jobs = ServeJobs::default();
        let mut view = ServeView::with_state(active_state(ServeMode::Tunnel), jobs.submitter());
        view.running_persists = Some(true);
        for persists in [false, true] {
            crate::session::config::update_config(|config| config.auth.persist_sessions = persists)
                .unwrap();
            view.pending_confirm = None;
            view.show_help = false;
            view.handle_key(key(code));
            wait_assessment(&mut view);
            assert_eq!(view.restart_probe.persists, persists, "{code}");
            assert_eq!(view.show_help, code == '?');
        }
    }
}

#[test]
#[serial_test::serial]
fn requesting_a_new_passphrase_does_not_overwrite_saved_credentials() {
    let _home = crate::session::test_support::isolate_app_dir();
    save_passphrase_to_disk("running daemon phrase");
    let mut jobs = ServeJobs::default();
    let mut view = ServeView::with_state(active_state(ServeMode::Tunnel), jobs.submitter());
    view.target = Some(crate::cli::serve::tui_control_target().unwrap());
    view.handle_key(key('G'));
    wait_assessment(&mut view);
    view.handle_key(key('G'));
    wait_assessment(&mut view);
    assert!(view.take_launch().is_some());
    assert_ne!(view.pending_passphrase, "running daemon phrase");
    assert_eq!(
        std::fs::read_to_string(
            crate::session::get_app_dir()
                .unwrap()
                .join("serve.saved_passphrase")
        )
        .unwrap(),
        "running daemon phrase"
    );
}

#[test]
#[serial_test::serial]
fn restart_requires_an_uninterrupted_confirmation() {
    let _home = crate::session::test_support::isolate_app_dir();
    for (cancelled, expired) in [(false, false), (true, false), (false, true)] {
        let mut jobs = ServeJobs::default();
        let mut view = ServeView::with_state(active_state(ServeMode::Local), jobs.submitter());
        view.target = Some(crate::cli::serve::tui_control_target().unwrap());
        view.handle_key(key('R'));
        wait_assessment(&mut view);
        assert!(matches!(
            view.pending_confirm,
            Some((PendingConfirm::Restart, _))
        ));
        if cancelled {
            view.handle_key(key('x'));
        }
        if expired {
            view.pending_confirm = Some((PendingConfirm::Restart, Instant::now() - CONFIRM_TTL));
        }
        view.handle_key(key('R'));
        wait_assessment(&mut view);
        if cancelled || expired {
            assert!(view.take_launch().is_none());
            assert!(matches!(
                view.pending_confirm,
                Some((PendingConfirm::Restart, _))
            ));
            assert!(matches!(view.state, ServeViewState::Active { .. }));
        } else {
            assert!(view.take_launch().is_some());
            assert!(matches!(view.state, ServeViewState::Starting { .. }));
        }
    }
}

#[test]
fn token_age_counts_only_under_token_auth() {
    let recorded = |auth: &str, core_only: bool| -> ServeLaunch {
        serde_json::from_value(serde_json::json!({
            "schema": 4, "pid": 1, "profile": "", "core_only": core_only,
            "host": "127.0.0.1", "port": 8080, "auth_mode": auth,
            "has_passphrase": false, "behind_proxy": false, "read_only": false,
            "remote": false, "tunnel_name": null, "tunnel_url": null,
            "no_tailscale": false,
        }))
        .unwrap()
    };
    for (launch, expected) in [
        (None, TokenWindow::Renews),
        (Some(recorded("token", false)), TokenWindow::Renews),
        (Some(recorded("none", true)), TokenWindow::Renews),
        (Some(recorded("passphrase", false)), TokenWindow::Reused),
        (Some(recorded("none", false)), TokenWindow::Reused),
    ] {
        assert_eq!(
            effective_token_window(launch.as_ref(), TokenWindow::Renews),
            expected
        );
    }
}

#[test]
#[serial_test::serial]
fn second_restart_key_reprobes_and_reconfirms_when_the_effect_changed() {
    let _home = crate::session::test_support::isolate_app_dir();
    let token = crate::session::get_app_dir().unwrap().join("serve.token");
    let mut jobs = ServeJobs::default();
    let mut view = ServeView::with_state(active_state(ServeMode::Local), jobs.submitter());
    view.target = Some(crate::cli::serve::tui_control_target().unwrap());
    let write_token = |age| {
        std::fs::write(&token, "t").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&token)
            .unwrap()
            .set_modified(std::time::SystemTime::now() - age)
            .unwrap();
    };
    write_token(Duration::from_secs(23 * 60 * 60 + 60));
    view.handle_key(key('R'));
    wait_assessment(&mut view);
    assert_eq!(view.restart_probe.local_token, TokenWindow::Boundary);
    write_token(Duration::from_secs(24 * 60 * 60 + 60));
    view.handle_key(key('R'));
    wait_assessment(&mut view);
    assert_eq!(view.restart_probe.local_token, TokenWindow::Renews);
    assert!(view.take_launch().is_none());
    assert!(view.pending_confirm.is_some());
    view.handle_key(key('R'));
    wait_assessment(&mut view);
    assert!(view.take_launch().is_some());
}

#[test]
#[serial_test::serial]
fn held_work_leaves_keys_clicks_ticks_and_render_usable() {
    let _home = crate::session::test_support::isolate_app_dir();
    for (held, code) in [
        (Held::Open, None),
        (Held::Picker, None),
        (Held::Providers, Some('T')),
        (Held::Providers, Some('R')),
        (Held::Retry, Some('P')),
        (Held::Reset, Some('R')),
        (Held::Restart, Some('G')),
        (Held::Restart, Some('R')),
        (Held::Restart, Some('?')),
        (Held::Active, None),
    ] {
        let mut jobs = ServeJobs::default();
        let (submitter, gate) = gated(&mut jobs, held);
        let state = match (held, code) {
            (Held::Open, _) => ServeViewState::Checking { attempt: 1 },
            (Held::Picker, _) | (Held::Providers, Some('T')) => picker_state(),
            (Held::Providers, _) => confirm_state(),
            (Held::Retry | Held::Reset, _) => error_state(),
            _ => active_state(ServeMode::Tunnel),
        };
        let mut view = ServeView::with_state(state, submitter);
        view.target = Some(crate::cli::serve::tui_control_target().unwrap());
        if held == Held::Active {
            let ServeViewState::Active { passphrase, .. } = &mut view.state else {
                unreachable!()
            };
            *passphrase = None;
        }
        let (exercise, exercise_rx) = mpsc::sync_channel(1);
        let (finished, finished_rx) = mpsc::sync_channel(1);
        let ui = std::thread::spawn(move || {
            match held {
                Held::Open => view.request(Job::Open, PendingJob::Open),
                Held::Picker => {
                    view.begin_stop();
                    view.finish_stop(Ok(()));
                }
                Held::Active => {
                    view.tick();
                }
                _ => {
                    view.handle_key(key(code.unwrap()));
                }
            }
            exercise_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            view.tick();
            assert!(matches!(
                view.handle_key(key(code.filter(|_| held == Held::Restart).unwrap_or('x'))),
                ServeAction::Continue
            ));
            let buffer = crate::tui::dialogs::test_render::draw(110, 40, |frame, theme| {
                view.render(frame, frame.area(), theme)
            });
            if held == Held::Restart {
                let text: String = buffer.content.iter().map(|cell| cell.symbol()).collect();
                assert!(text.contains("https://host.tailnet.ts.net/"));
                assert!(text.contains("running daemon phrase"));
                let mouse = view.mouse.borrow();
                for expected in ["https://host.tailnet.ts.net/", "running daemon phrase"] {
                    assert!(mouse.copies.iter().any(|((_, value), _)| value == expected));
                }
                if cfg!(feature = "web") {
                    assert!(text.chars().any(|cell| matches!(cell, '▀' | '▄' | '█')));
                }
            }
            let close_hint = if matches!(held, Held::Active | Held::Restart) {
                "Esc: close"
            } else {
                "[Esc] cancel"
            };
            let (col, row) = crate::tui::dialogs::test_render::find(&buffer, close_hint);
            let close = view
                .handle_click(col, row)
                .expect("pending view retains its cancel target");
            assert!(matches!(view.handle_key(close), ServeAction::Close));
            drop(view);
            finished.send(()).unwrap();
        });
        gate.entered(held);
        exercise.send(()).unwrap();
        finished_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("UI waited for held work");
        let mut reopened = ServeView::new(&mut jobs);
        gate.release(if held == Held::Active {
            JobResult::Active {
                log_offset: 0,
                passphrase: Some("late old credential".to_owned()),
            }
        } else {
            JobResult::Failed("late old failure".to_owned())
        });
        if held == Held::Open {
            gate.entered(Held::Open);
            gate.release(snapshot());
        }
        wait_for(&mut reopened, |view| {
            matches!(view.state, ServeViewState::ModePicker { .. })
        });
        assert_eq!(reopened.local_primary, "fresh interface");
        ui.join().unwrap();
    }
}

#[test]
#[serial_test::serial]
fn refresh_replaces_old_results_and_mode_change_invalidates_assessment() {
    let _home = crate::session::test_support::isolate_app_dir();
    let mut jobs = ServeJobs::default();
    let (submitter, gate) = gated(&mut jobs, Held::Providers);
    let mut view = ServeView::with_state(confirm_state(), submitter);
    view.handle_key(key('R'));
    gate.entered(Held::Providers);
    view.handle_key(key('R'));
    gate.release(JobResult::Providers(
        TransportStatus::NotInstalled,
        TransportStatus::NotInstalled,
    ));
    gate.entered(Held::Providers);
    gate.release(JobResult::Providers(
        TransportStatus::Ready,
        TransportStatus::Ready,
    ));
    wait_for(&mut view, |view| {
        matches!(
            view.state,
            ServeViewState::Confirm {
                tailscale: TransportStatus::Ready,
                cloudflare: TransportStatus::Ready,
                flash: Some(_),
                ..
            }
        )
    });
    assert!(matches!(
        view.state,
        ServeViewState::Confirm {
            selected: TunnelTransport::Cloudflare,
            ..
        }
    ));
    view.handle_key(key('R'));
    gate.entered(Held::Providers);
    view.handle_key(key('L'));
    gate.release(JobResult::Providers(
        TransportStatus::NotInstalled,
        TransportStatus::NotInstalled,
    ));
    wait_for(&mut view, |view| {
        matches!(
            view.state,
            ServeViewState::ModePicker {
                selected: ServeMode::Local,
                ..
            }
        )
    });
}

#[test]
#[serial_test::serial]
fn a_key_during_second_restart_assessment_cancels_the_confirmation() {
    let _home = crate::session::test_support::isolate_app_dir();
    let mut jobs = ServeJobs::default();
    let (submitter, gate) = gated(&mut jobs, Held::Restart);
    let mut view = ServeView::with_state(active_state(ServeMode::Local), submitter);
    view.handle_key(key('R'));
    gate.entered(Held::Restart);
    gate.release(JobResult::Restart(RestartProbe::default()));
    wait_for(&mut view, |view| view.pending_confirm.is_some());
    view.handle_key(key('R'));
    gate.entered(Held::Restart);
    view.handle_key(key('x'));
    gate.release(JobResult::Restart(RestartProbe::default()));
    view.handle_key(key('R'));
    gate.entered(Held::Restart);
    assert!(view.take_launch().is_none());
    gate.release(JobResult::Restart(RestartProbe::default()));
    wait_for(&mut view, |view| view.pending_confirm.is_some());
    assert!(view.take_launch().is_none());
}

#[test]
#[serial_test::serial]
fn a_timely_confirmation_survives_a_slow_second_assessment() {
    let _home = crate::session::test_support::isolate_app_dir();
    for (code, mode) in [('R', ServeMode::Local), ('G', ServeMode::Tunnel)] {
        let mut jobs = ServeJobs::default();
        let (submitter, gate) = gated(&mut jobs, Held::Restart);
        let mut view = ServeView::with_state(active_state(mode), submitter);
        view.target = Some(crate::cli::serve::tui_control_target().unwrap());
        view.handle_key(key(code));
        gate.entered(Held::Restart);
        gate.release(JobResult::Restart(RestartProbe::default()));
        wait_for(&mut view, |view| view.pending_confirm.is_some());
        let confirmed_at = view.pending_confirm.unwrap().1;

        view.handle_key(key(code));
        gate.entered(Held::Restart);
        while let Some(remaining) = CONFIRM_TTL.checked_sub(confirmed_at.elapsed()) {
            std::thread::park_timeout(remaining + Duration::from_millis(1));
        }
        view.tick();
        assert!(view.take_launch().is_none());

        gate.release(JobResult::Restart(RestartProbe::default()));
        wait_assessment(&mut view);
        assert!(
            view.take_launch().is_some(),
            "{code} discarded a timely confirmation after its assessment"
        );
        assert!(view.pending_confirm.is_none());
    }
}

#[test]
#[serial_test::serial]
fn every_pending_state_leaves_a_closed_worker() {
    let _home = crate::session::test_support::isolate_app_dir();
    for (held, recovery_key) in [
        (Held::Open, None),
        (Held::Picker, None),
        (Held::Providers, None),
        (Held::Retry, None),
        (Held::Reset, None),
        (Held::Restart, None),
        (Held::LaunchContext, None),
        (Held::LaunchOutcome, None),
        (Held::Starting, None),
        (Held::Passphrase, None),
        (Held::ErrorContext, None),
        (Held::ErrorContext, Some('P')),
        (Held::ErrorContext, Some('R')),
        (Held::Active, None),
        (Held::Copy, None),
    ] {
        let mut view = ServeView::for_test(if held == Held::ErrorContext {
            error_state()
        } else {
            active_state(ServeMode::Tunnel)
        });
        if held == Held::Active {
            let ServeViewState::Active { passphrase, .. } = &mut view.state else {
                unreachable!()
            };
            *passphrase = None;
        }
        let target = crate::cli::serve::tui_control_target().unwrap();
        let (job, pending) = match held {
            Held::Open => (Job::Open, PendingJob::Open),
            Held::Picker => (Job::Picker, PendingJob::Picker { selected: None }),
            Held::Providers => (
                Job::Providers,
                PendingJob::Providers {
                    selected: None,
                    failed: None,
                    refreshed: true,
                },
            ),
            Held::Retry => (
                Job::Retry(target.clone()),
                PendingJob::Retry {
                    failed: None,
                    target,
                },
            ),
            Held::Reset => (Job::Reset(target), PendingJob::Reset),
            Held::Restart => (
                Job::Restart(ServeMode::Tunnel, None),
                PendingJob::Restart {
                    intent: RestartIntent::Help,
                    confirmed: None,
                },
            ),
            Held::LaunchContext => (
                Job::LaunchContext {
                    needs_passphrase: false,
                },
                PendingJob::LaunchContext(ServeLaunchRequest {
                    intent: LaunchIntent::Start(None),
                    mode: ServeMode::Local,
                    transport: None,
                    passphrase: None,
                }),
            ),
            Held::LaunchOutcome => (
                Job::LaunchOutcome {
                    target,
                    mode: ServeMode::Local,
                    transport: None,
                    passphrase: None,
                    log_offset: 0,
                },
                PendingJob::LaunchOutcome,
            ),
            Held::Starting => (Job::ObserveStarting(target), PendingJob::Starting),
            Held::Passphrase => (Job::Passphrase, PendingJob::Passphrase),
            Held::ErrorContext => (
                Job::ErrorContext,
                match recovery_key {
                    Some('P') => PendingJob::RetryContext { failed: None },
                    Some('R') => PendingJob::ResetContext,
                    _ => PendingJob::ErrorContext,
                },
            ),
            Held::Active => (Job::ObserveActive(Some(target)), PendingJob::Active),
            Held::Copy => (Job::Copy("URL", "fixture".to_owned()), PendingJob::Copy),
            Held::Stop => panic!("App-owned stop has no view pending state"),
        };
        view.request(job, pending);
        view.tick();
        assert!(view.pending.is_none(), "{held:?}");
        assert!(!view.launch_in_flight, "{held:?}");
        assert!(
            matches!(view.state, ServeViewState::Error { .. }),
            "{held:?}"
        );
        if held == Held::ErrorContext {
            assert!(matches!(&view.state, ServeViewState::Error { message, .. }
                    if message == "tailscale funnel failed"));
        }
    }
}

#[test]
#[serial_test::serial]
fn a_liveness_result_cannot_beat_a_published_url() {
    let _home = crate::session::test_support::isolate_app_dir();
    for published in [false, true] {
        let mut jobs = ServeJobs::default();
        let (submitter, gate) = gated(&mut jobs, Held::Starting);
        let mut view = ServeView::with_state(starting(), submitter);
        view.target = Some(crate::cli::serve::tui_control_target().unwrap());
        view.tick();
        gate.entered(Held::Starting);
        for _ in 0..5 {
            view.tick();
        }
        gate.release(JobResult::Starting(work::StartingObservation {
            running: false,
            target: crate::cli::serve::tui_control_target().unwrap(),
            urls: if published {
                vec![ServeUrl {
                    label: None,
                    url: "http://127.0.0.1:8080/".to_owned(),
                }]
            } else {
                Vec::new()
            },
            passphrase: None,
            log_offset: 0,
            tail: Default::default(),
        }));
        wait_for(&mut view, |view| {
            !matches!(view.state, ServeViewState::Starting { .. })
        });
        assert_eq!(
            matches!(view.state, ServeViewState::Active { .. }),
            published
        );
        assert_eq!(
            matches!(view.state, ServeViewState::Error { .. }),
            !published
        );
    }
}

#[test]
#[serial_test::serial]
fn a_reopened_view_waits_for_its_owned_launch_and_ignores_an_old_stop() {
    let _home = crate::session::test_support::isolate_app_dir();
    let mut jobs = ServeJobs::default();
    let started = Instant::now();
    let mut view = ServeView::for_launch(DashboardLaunch::tunnel_for_test(started), &mut jobs);
    view.tick();
    assert!(
        matches!(view.state, ServeViewState::Starting { started_at, .. } if started_at == started)
    );
    view.finish_stop(Ok(()));
    assert!(
        matches!(view.state, ServeViewState::Starting { started_at, .. } if started_at == started)
    );
    assert!(view.launch_in_flight);
    view.resume_owned_stop(started + Duration::from_secs(1));
    view.finish_stop(Err("another launch stopped".to_owned()));
    assert!(
        matches!(view.state, ServeViewState::Starting { started_at, .. } if started_at == started)
    );
    view.resume_owned_stop(started);
    view.finish_stop(Err("owned launch stop failed".to_owned()));
    assert!(matches!(view.state, ServeViewState::Error { .. }));
    assert!(!view.launch_in_flight);
}

#[test]
#[serial_test::serial]
fn a_failed_stop_of_a_reopened_launch_loads_identity_before_recovery() {
    let _home = crate::session::test_support::isolate_app_dir();
    for (code, held) in [('P', Held::Retry), ('R', Held::Reset)] {
        let mut jobs = ServeJobs::default();
        let (_submitter, gate) = gated(&mut jobs, held);
        let mut view =
            ServeView::for_launch(DashboardLaunch::tunnel_for_test(Instant::now()), &mut jobs);
        assert!(matches!(
            view.handle_key(key('S')),
            ServeAction::StopOwnedLaunch
        ));
        view.begin_stop();
        view.finish_stop(Err("tailscale funnel failed".to_owned()));
        wait_for(&mut view, |view| view.pending.is_none());
        assert!(matches!(&view.state, ServeViewState::Error { message, .. }
            if message == "tailscale funnel failed"));
        view.handle_key(key(code));
        assert!(view.pending.is_some(), "{code} recovery did not start");
        gate.entered(held);
        gate.release(if code == 'P' {
            JobResult::Retried {
                tailscale: TransportStatus::Ready,
                cloudflare: TransportStatus::Ready,
                target: crate::cli::serve::tui_control_target().unwrap(),
            }
        } else {
            JobResult::Reset(Err("fixture reset outcome unknown".to_owned()))
        });
        wait_for(&mut view, |view| view.pending.is_none());
        if code == 'P' {
            assert!(matches!(
                view.state,
                ServeViewState::Confirm {
                    selected: TunnelTransport::Cloudflare,
                    ..
                }
            ));
        } else {
            assert!(matches!(&view.state, ServeViewState::Error { message, .. }
                if message.contains("fixture reset outcome unknown")));
        }
    }
}

struct FunnelFixture {
    root: tempfile::TempDir,
    _path: crate::session::test_support::EnvGuard,
    _environment: crate::session::test_support::EnvGuard,
}

impl FunnelFixture {
    fn new() -> Self {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let program = root.path().join("tailscale");
        std::fs::write(
            &program,
            r#"#!/bin/sh
case "$1 $2" in
  "funnel reset")
    printf 'reset\n' >> "$AOE_TEST_FUNNEL_ROOT/calls"
    if [ -f "$AOE_TEST_FUNNEL_ROOT/spawn-descendant" ]; then
      /bin/sh -c '
        printf "%s\n" "$$" > "$AOE_TEST_FUNNEL_ROOT/descendant"
        printf "ready\n" > "$AOE_TEST_FUNNEL_ROOT/descendant-ready"
        read -r line < "$AOE_TEST_FUNNEL_ROOT/descendant-release"
      ' &
      read -r ready < "$AOE_TEST_FUNNEL_ROOT/descendant-ready"
    fi
    printf '%s\n' "$$" > "$AOE_TEST_FUNNEL_ROOT/entered"
    read -r line < "$AOE_TEST_FUNNEL_ROOT/release"
    printf 'complete\n' > "$AOE_TEST_FUNNEL_ROOT/complete"
    exit 0 ;;
  "status --json")
    printf '{"Self":{"CapMap":{"https://tailscale.com/cap/funnel-ports?ports=443":null}}}\n'
    exit 0 ;;
  "--version ")
    if [ -f "$AOE_TEST_FUNNEL_ROOT/hold-probe" ]; then
      printf '%s\n' "$$" > "$AOE_TEST_FUNNEL_ROOT/entered"
      read -r line < "$AOE_TEST_FUNNEL_ROOT/release"
    fi
    exit 0 ;;
  "status ") exit 0 ;;
  *) exit 1 ;;
esac
"#,
        )
        .unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        let cloudflare = root.path().join("cloudflared");
        std::fs::write(&cloudflare, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&cloudflare, std::fs::Permissions::from_mode(0o755)).unwrap();
        for name in ["release", "descendant-ready", "descendant-release"] {
            nix::unistd::mkfifo(
                &root.path().join(name),
                nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
            )
            .unwrap();
        }
        let path = crate::session::test_support::path_prepended(root.path());
        let environment = crate::session::test_support::EnvGuard::set(&[(
            "AOE_TEST_FUNNEL_ROOT",
            root.path().as_os_str(),
        )]);
        Self {
            root,
            _path: path,
            _environment: environment,
        }
    }

    fn entered(&self) -> crate::process::CapturedProcess {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(pid) = std::fs::read_to_string(self.root.path().join("entered"))
                .ok()
                .and_then(|raw| raw.trim().parse::<u32>().ok())
            {
                let birth = crate::process::process_birth_identity(pid)
                    .unwrap()
                    .unwrap();
                return crate::process::CapturedProcess::capture(pid, &birth)
                    .unwrap()
                    .unwrap();
            }
            assert!(Instant::now() < deadline, "reset fixture did not enter");
            std::thread::yield_now();
        }
    }

    fn release(&self) {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match std::fs::OpenOptions::new()
                .write(true)
                .custom_flags(nix::libc::O_NONBLOCK)
                .open(self.root.path().join("release"))
            {
                Ok(mut file) => {
                    file.write_all(b"continue\n").unwrap();
                    return;
                }
                Err(error) if error.raw_os_error() == Some(nix::libc::ENXIO) => {
                    assert!(
                        Instant::now() < deadline,
                        "reset fixture did not open its gate"
                    );
                    std::thread::yield_now();
                }
                Err(error) => panic!("cannot release reset: {error}"),
            }
        }
    }
}

#[test]
#[serial_test::serial]
fn a_superseded_queued_reset_never_executes() {
    let _home = crate::session::test_support::isolate_app_dir();
    let fixture = FunnelFixture::new();
    let mut jobs = ServeJobs::default();
    let (submitter, gate) = gated(&mut jobs, Held::Providers);
    let blocker = submitter.request(1, Job::Providers);
    gate.entered(Held::Providers);
    let mut view = ServeView::with_state(error_state(), submitter);
    view.target = Some(crate::cli::serve::tui_control_target().unwrap());
    view.handle_key(key('R'));
    assert!(
        view.pending
            .as_ref()
            .is_some_and(|pending| { matches!(pending.kind, PendingJob::Reset) }),
        "R did not queue its reset before cancellation"
    );
    assert!(matches!(
        view.handle_key(KeyEvent::from(KeyCode::Esc)),
        ServeAction::Close
    ));
    drop(view);
    let mut reopened = ServeView::new(&mut jobs);
    gate.release(JobResult::Providers(
        TransportStatus::Ready,
        TransportStatus::Ready,
    ));
    wait_for(&mut reopened, |view| {
        matches!(view.state, ServeViewState::ModePicker { .. })
    });
    assert!(!fixture.root.path().join("calls").exists());
    drop(blocker);
}

#[test]
#[serial_test::serial]
fn an_admitted_reset_finishes_once_after_close_and_reopen() {
    let _home = crate::session::test_support::isolate_app_dir();
    let fixture = FunnelFixture::new();
    std::fs::write(fixture.root.path().join("spawn-descendant"), "").unwrap();
    let mut jobs = ServeJobs::default();
    let mut view = ServeView::with_state(error_state(), jobs.submitter());
    view.target = Some(crate::cli::serve::tui_control_target().unwrap());
    view.handle_key(key('R'));
    let process = fixture.entered();
    let descendant_pid = std::fs::read_to_string(fixture.root.path().join("descendant"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let birth = crate::process::process_birth_identity(descendant_pid)
        .unwrap()
        .unwrap();
    let descendant = crate::process::CapturedProcess::capture(descendant_pid, &birth)
        .unwrap()
        .unwrap();
    assert!(!process.exited().unwrap());
    assert!(!descendant.exited().unwrap());
    for _ in 0..3 {
        view.tick();
        assert!(matches!(view.handle_key(key('R')), ServeAction::Continue));
    }
    let buffer = crate::tui::dialogs::test_render::draw(110, 40, |frame, theme| {
        view.render(frame, frame.area(), theme)
    });
    let (col, row) = crate::tui::dialogs::test_render::find(&buffer, "[Esc] cancel");
    let close = view.handle_click(col, row).unwrap();
    assert!(matches!(view.handle_key(close), ServeAction::Close));
    drop(view);
    let mut reopened = ServeView::new(&mut jobs);
    assert!(!process.exited().unwrap());
    assert!(!descendant.exited().unwrap());
    assert!(!fixture.root.path().join("complete").exists());
    fixture.release();
    wait_for(&mut reopened, |view| {
        matches!(view.state, ServeViewState::ModePicker { .. })
    });
    assert!(process.exited().unwrap());
    assert!(descendant.exited().unwrap());
    assert_eq!(
        std::fs::read_to_string(fixture.root.path().join("calls")).unwrap(),
        "reset\n"
    );
    assert_eq!(
        std::fs::read_to_string(fixture.root.path().join("complete")).unwrap(),
        "complete\n"
    );
}

#[test]
#[serial_test::serial]
fn a_reset_deadline_reaps_its_process_and_keeps_outcome_uncertain() {
    let _home = crate::session::test_support::isolate_app_dir();
    let fixture = FunnelFixture::new();
    let mut jobs = ServeJobs::default();
    let mut view = ServeView::with_state(error_state(), jobs.submitter());
    view.target = Some(crate::cli::serve::tui_control_target().unwrap());
    view.handle_key(key('R'));
    let process = fixture.entered();
    wait_for(&mut view, |view| view.pending.is_none());
    assert!(process.exited().unwrap());
    assert!(!fixture.root.path().join("complete").exists());
    assert!(
        matches!(&view.state, ServeViewState::Error { message, .. } if message.contains("Funnel state may have changed"))
    );
    assert!(matches!(
        view.handle_key(KeyEvent::from(KeyCode::Esc)),
        ServeAction::Close
    ));
}

#[test]
#[serial_test::serial]
fn shutdown_cancels_and_reaps_a_retained_probe_without_detaching_its_owner() {
    let _home = crate::session::test_support::isolate_app_dir();
    let fixture = FunnelFixture::new();
    std::fs::write(fixture.root.path().join("hold-probe"), "").unwrap();
    let mut jobs = ServeJobs::default();
    let view = ServeView::new(&mut jobs);
    let process = fixture.entered();
    drop(view);
    let (finished, received) = mpsc::sync_channel(1);
    let shutdown = std::thread::spawn(move || {
        jobs.shutdown();
        finished.send(()).unwrap();
    });
    received
        .recv_timeout(Duration::from_secs(2))
        .expect("Serve shutdown waited for the probe deadline");
    assert!(process.exited().unwrap());
    shutdown.join().unwrap();
}

#[test]
#[serial_test::serial]
fn an_initial_result_cannot_replace_a_new_launch_state() {
    let _home = crate::session::test_support::isolate_app_dir();
    let mut jobs = ServeJobs::default();
    let (submitter, gate) = gated(&mut jobs, Held::Open);
    let mut view = ServeView::with_state(ServeViewState::Checking { attempt: 1 }, submitter);
    view.request(Job::Open, PendingJob::Open);
    gate.entered(Held::Open);
    view.state = starting();
    view.launch_in_flight = true;
    gate.release(snapshot());
    wait_for(&mut view, |view| view.pending.is_none());
    assert!(matches!(view.state, ServeViewState::Starting { .. }));
    assert!(view.launch_in_flight);
}

#[test]
#[serial_test::serial]
fn provider_retry_and_refresh_use_fresh_assessments_before_launch() {
    let _home = crate::session::test_support::isolate_app_dir();
    let fixture = FunnelFixture::new();
    let mut jobs = ServeJobs::default();
    let mut view = ServeView::with_state(error_state(), jobs.submitter());
    view.target = Some(crate::cli::serve::tui_control_target().unwrap());
    view.handle_key(key('P'));
    wait_for(&mut view, |view| {
        matches!(
            view.state,
            ServeViewState::Confirm {
                selected: TunnelTransport::Cloudflare,
                tailscale: TransportStatus::Ready,
                cloudflare: TransportStatus::Ready,
                ..
            }
        )
    });
    std::fs::write(
        fixture.root.path().join("cloudflared"),
        "#!/bin/sh\nexit 1\n",
    )
    .unwrap();
    view.handle_key(key('R'));
    wait_for(&mut view, |view| {
        matches!(
            view.state,
            ServeViewState::Confirm {
                selected: TunnelTransport::Cloudflare,
                cloudflare: TransportStatus::NotInstalled,
                ..
            }
        )
    });
    assert!(matches!(
        view.handle_key(KeyEvent::from(KeyCode::Enter)),
        ServeAction::Continue
    ));
    assert!(view.take_launch().is_none());
    assert!(!view.launch_in_flight);
}

#[test]
#[serial_test::serial]
fn provider_retry_rediscovers_identity_after_metadata_is_repaired() {
    let _home = crate::session::test_support::isolate_app_dir();
    let fixture = FunnelFixture::new();
    let pid = crate::session::get_app_dir().unwrap().join("serve.pid");
    for repaired in [false, true] {
        std::fs::create_dir(&pid).unwrap();
        let mut jobs = ServeJobs::default();
        let mut view = ServeView::with_state(error_state(), jobs.submitter());
        view.request(Job::ErrorContext, PendingJob::ErrorContext);
        wait_for(&mut view, |view| view.pending.is_none());
        assert!(view.target.is_none());
        assert!(view.control_error.is_some());
        if repaired {
            std::fs::remove_dir(&pid).unwrap();
        }
        view.handle_key(key('P'));
        wait_for(&mut view, |view| view.pending.is_none());
        if repaired {
            assert!(matches!(view.state, ServeViewState::Confirm { .. }));
        } else {
            assert!(matches!(&view.state, ServeViewState::Error { message, .. }
                if message == "tailscale funnel failed"));
            assert!(view.target.is_none());
            assert!(view.control_error.is_some());
            assert!(!fixture.root.path().join("calls").exists());
            std::fs::remove_dir(&pid).unwrap();
        }
    }
}

#[test]
#[serial_test::serial]
fn quiet_startup_repaints_when_the_elapsed_second_changes() {
    let _home = crate::session::test_support::isolate_app_dir();
    for owned_launch in [false, true] {
        let mut jobs = ServeJobs::default();
        let (submitter, gate) = gated(&mut jobs, Held::Starting);
        let mut view = ServeView::with_state(starting(), submitter);
        view.launch_in_flight = owned_launch;
        if !owned_launch {
            view.request(
                Job::ObserveStarting(crate::cli::serve::tui_control_target().unwrap()),
                PendingJob::Starting,
            );
            gate.entered(Held::Starting);
        }
        view.tick();
        let screen = |view: &ServeView| {
            crate::tui::dialogs::test_render::text(&crate::tui::dialogs::test_render::draw(
                110,
                40,
                |frame, theme| view.render(frame, frame.area(), theme),
            ))
        };
        let seconds = |text: String| {
            text.split("Elapsed: ")
                .nth(1)
                .unwrap()
                .split('s')
                .next()
                .unwrap()
                .parse::<u64>()
                .unwrap()
        };
        let before = seconds(screen(&view));
        if let ServeViewState::Starting { started_at, .. } = &mut view.state {
            *started_at -= Duration::from_secs(2);
        }
        let repaint = view.tick();
        let after = seconds(screen(&view));
        drop(view);
        if !owned_launch {
            gate.release(JobResult::Failed(
                "released cancelled observation".to_owned(),
            ));
        }
        drop(gate);
        jobs.shutdown();
        assert!(after > before);
        assert!(
            repaint,
            "elapsed clock must repaint; owned_launch={owned_launch}"
        );
    }
}

#[test]
#[serial_test::serial]
fn launch_observations_never_mix_a_replacement_with_prior_credentials() {
    use std::os::unix::process::CommandExt;

    struct ChildGuard(std::process::Child);
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            if matches!(self.0.try_wait(), Ok(None)) {
                let _ = self.0.kill();
            }
            let _ = self.0.wait();
        }
    }

    let _home = crate::session::test_support::isolate_app_dir();
    let app = crate::session::get_app_dir().unwrap();
    for hold_starting in [false, true] {
        for replaced in [false, true] {
            remove_app_file("serve.url");
            std::fs::write(app.join("serve.mode"), "tunnel").unwrap();
            std::fs::write(app.join("serve.passphrase"), "launch A credential").unwrap();
            let fixture = tempfile::tempdir().unwrap();
            nix::unistd::mkfifo(
                &fixture.path().join("serve"),
                nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
            )
            .unwrap();
            let spawn = || {
                ChildGuard(
                    std::process::Command::new("cat")
                        .arg0("aoe")
                        .arg("serve")
                        .current_dir(fixture.path())
                        .stdin(std::process::Stdio::null())
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .spawn()
                        .unwrap(),
                )
            };
            let mut original = spawn();
            std::fs::write(app.join("serve.pid"), original.0.id().to_string()).unwrap();
            let target = crate::cli::serve::wait_for_test_daemon(&mut original.0);
            let (entered, received) = mpsc::sync_channel(1);
            let (release, retained) = mpsc::sync_channel(1);
            let mut jobs = ServeJobs::default();
            let submitter = jobs.with_handler(move |job, control, runtime| {
                let held = if hold_starting {
                    Held::Starting
                } else {
                    Held::LaunchOutcome
                };
                if operation(&job) == held {
                    entered.send(()).unwrap();
                    retained.recv_timeout(Duration::from_secs(10)).unwrap();
                }
                run_job(job, control, runtime)
            });
            let mut view = ServeView::with_state(
                ServeViewState::Starting {
                    mode: ServeMode::Tunnel,
                    transport: Some(TunnelTransport::Tailscale),
                    passphrase: Some("launch A credential".to_owned()),
                    started_at: Instant::now(),
                    log_offset: 0,
                },
                submitter,
            );
            view.launch_in_flight = true;
            view.finish_launch(Ok(target), None);
            if hold_starting {
                wait_for(&mut view, |view| {
                    !view
                        .pending
                        .as_ref()
                        .is_some_and(|pending| matches!(pending.kind, PendingJob::LaunchOutcome))
                });
                if let ServeViewState::Error { message, .. } = &view.state {
                    panic!(
                        "launch failed before Starting observation; replaced={replaced}: {message}"
                    );
                }
            }
            received.recv_timeout(Duration::from_secs(10)).unwrap();
            let mut replacement = None;
            let url = if replaced {
                original.0.kill().unwrap();
                original.0.wait().unwrap();
                replacement = Some(spawn());
                std::fs::write(
                    app.join("serve.pid"),
                    replacement.as_ref().unwrap().0.id().to_string(),
                )
                .unwrap();
                crate::cli::serve::wait_for_test_daemon(&mut replacement.as_mut().unwrap().0);
                std::fs::write(app.join("serve.mode"), "local").unwrap();
                std::fs::write(app.join("serve.passphrase"), "replacement B credential").unwrap();
                "https://replacement.invalid/?token=b"
            } else {
                "https://original.invalid/?token=a"
            };
            std::fs::write(app.join("serve.url"), url).unwrap();
            release.send(()).unwrap();
            wait_for(&mut view, |view| {
                matches!(
                    view.state,
                    ServeViewState::Active { .. } | ServeViewState::Error { .. }
                )
            });
            if replaced {
                assert!(replacement
                    .as_mut()
                    .unwrap()
                    .0
                    .try_wait()
                    .unwrap()
                    .is_none());
                assert!(
                    matches!(view.state, ServeViewState::Error { .. }),
                    "must reject the replacement; hold_starting={hold_starting}"
                );
            } else {
                let ServeViewState::Active {
                    urls,
                    passphrase,
                    mode,
                    ..
                } = &view.state
                else {
                    let message = match &view.state {
                        ServeViewState::Error { message, .. } => message.as_str(),
                        _ => "view did not settle",
                    };
                    panic!("unchanged launch failed; hold_starting={hold_starting}: {message}");
                };
                assert_eq!(*mode, ServeMode::Tunnel, "hold_starting={hold_starting}");
                assert_eq!(urls[0].url, url, "hold_starting={hold_starting}");
                assert_eq!(
                    passphrase.as_deref(),
                    Some("launch A credential"),
                    "hold_starting={hold_starting}"
                );
            }
        }
    }
}

struct TestDaemon {
    _root: tempfile::TempDir,
    child: std::process::Child,
}

impl TestDaemon {
    fn new() -> Self {
        use std::os::unix::process::CommandExt;
        let root = tempfile::tempdir().unwrap();
        nix::unistd::mkfifo(
            &root.path().join("serve"),
            nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
        )
        .unwrap();
        let child = std::process::Command::new("cat")
            .arg0("aoe")
            .arg("serve")
            .current_dir(root.path())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        Self { _root: root, child }
    }

    fn publish(&mut self) -> TuiControlTarget {
        let app = crate::session::get_app_dir().unwrap();
        std::fs::write(app.join("serve.pid"), self.child.id().to_string()).unwrap();
        crate::cli::serve::wait_for_test_daemon(&mut self.child)
    }

    fn exit(&mut self) {
        self.child.kill().unwrap();
        self.child.wait().unwrap();
    }
}

impl Drop for TestDaemon {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

#[test]
#[serial_test::serial]
fn open_uses_current_credentials_without_saved_default_fallback() {
    let _home = crate::session::test_support::isolate_app_dir();
    let app = crate::session::get_app_dir().unwrap();
    let mut daemon = TestDaemon::new();
    daemon.publish();
    std::fs::write(app.join("serve.mode"), "tunnel").unwrap();
    std::fs::write(app.join("serve.url"), "https://current.invalid/").unwrap();
    save_passphrase_to_disk("previous saved default");
    for current in [Some("current runtime credential"), None] {
        remove_app_file("serve.passphrase");
        if let Some(current) = current {
            std::fs::write(app.join("serve.passphrase"), current).unwrap();
        }
        let mut jobs = ServeJobs::default();
        let mut view = ServeView::new(&mut jobs);
        wait_for(&mut view, |view| {
            matches!(
                view.state,
                ServeViewState::Active { .. } | ServeViewState::Error { .. }
            )
        });
        let ServeViewState::Active { passphrase, .. } = &view.state else {
            panic!("current Daemon was not opened");
        };
        assert_eq!(passphrase.as_deref(), current);
        assert_eq!(view.pending_passphrase, "previous saved default");
        if current.is_none() {
            crate::tui::dialogs::test_render::draw(110, 40, |frame, theme| {
                view.render(frame, frame.area(), theme)
            });
            assert!(view
                .mouse
                .borrow()
                .copies
                .iter()
                .all(|((label, _), _)| *label != "Passphrase"));
            let ServeViewState::Active { opened_at, .. } = view.state else {
                unreachable!()
            };
            std::fs::write(app.join("serve.passphrase"), "recovered runtime credential").unwrap();
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let changed = view.tick();
                if let ServeViewState::Active {
                    passphrase: Some(passphrase),
                    opened_at: current_opened_at,
                    ..
                } = &view.state
                {
                    assert_eq!(passphrase, "recovered runtime credential");
                    assert_eq!(*current_opened_at, opened_at);
                    assert!(changed, "credential recovery did not request repaint");
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "published runtime credential never became available without reopening"
                );
                std::thread::yield_now();
            }
            crate::tui::dialogs::test_render::draw(110, 40, |frame, theme| {
                view.render(frame, frame.area(), theme)
            });
            assert!(view
                .mouse
                .borrow()
                .copies
                .iter()
                .any(|((label, value), _)| {
                    *label == "Passphrase" && value == "recovered runtime credential"
                }));
            assert_eq!(view.pending_passphrase, "previous saved default");
            assert_eq!(
                read_app_file("serve.saved_passphrase").as_deref(),
                Some("previous saved default")
            );
        }
    }
}

#[test]
#[serial_test::serial]
fn active_credential_recovery_rejects_replacements_exit_and_disabled_gate() {
    use super::work::BEFORE_OBSERVATION_RECHECK;
    let _home = crate::session::test_support::isolate_app_dir();
    let app = crate::session::get_app_dir().unwrap();
    #[derive(Clone, Copy)]
    enum Change {
        ReplaceBeforeRead,
        ReplaceDuringRead,
        ExitDuringRead,
        NoPassphrase,
    }
    for change in [
        Change::ReplaceBeforeRead,
        Change::ReplaceDuringRead,
        Change::ExitDuringRead,
        Change::NoPassphrase,
    ] {
        remove_app_file("serve.launch");
        remove_app_file("serve.passphrase");
        let mut original = TestDaemon::new();
        original.publish();
        std::fs::write(app.join("serve.mode"), "tunnel").unwrap();
        std::fs::write(app.join("serve.url"), "https://original.invalid/").unwrap();
        let (entered, received) = mpsc::sync_channel(1);
        let (release, retained) = mpsc::sync_channel(1);
        let (observed, next_observation) = mpsc::sync_channel(1);
        let runtime_passphrase = app.join("serve.passphrase");
        let mut pause: Option<Box<dyn FnOnce() + Send>> = Some(Box::new(move || {
            entered.send(()).unwrap();
            retained.recv_timeout(Duration::from_secs(10)).unwrap();
        }));
        let mut jobs = ServeJobs::default();
        let submitter = jobs.with_handler(move |job, control, runtime| {
            if matches!(job, Job::ObserveActive(_)) {
                if let Some(pause) = pause.take() {
                    std::fs::write(&runtime_passphrase, "original runtime credential").unwrap();
                    if matches!(change, Change::ReplaceDuringRead | Change::ExitDuringRead) {
                        BEFORE_OBSERVATION_RECHECK.with(|hook| *hook.borrow_mut() = Some(pause));
                    } else {
                        pause();
                    }
                    if matches!(change, Change::NoPassphrase) {
                        let Job::ObserveActive(Some(target)) = &job else {
                            panic!("missing captured target for credential recovery");
                        };
                        let lease = runtime
                            .block_on(crate::cli::serve::TuiObservationLease::prepare(
                                Some(target.clone()),
                                &control.cancel,
                            ))
                            .unwrap();
                        assert!(lease.is_running());
                        assert!(!lease.has_passphrase());
                    }
                } else {
                    let _ = observed.try_send(());
                }
            }
            run_job(job, control, runtime)
        });
        let mut view = ServeView::with_state(ServeViewState::Checking { attempt: 0 }, submitter);
        view.request(Job::Open, PendingJob::Open);
        view.state = ServeViewState::Checking {
            attempt: view.last_attempt,
        };
        wait_for(&mut view, |view| {
            matches!(view.state, ServeViewState::Active { .. })
        });
        assert!(matches!(
            &view.state,
            ServeViewState::Active {
                passphrase: None,
                ..
            }
        ));
        wait_for(&mut view, |_| received.try_recv().is_ok());
        let mut replacement = None;
        match change {
            Change::ReplaceBeforeRead | Change::ReplaceDuringRead => {
                original.exit();
                let mut successor = TestDaemon::new();
                successor.publish();
                std::fs::write(app.join("serve.passphrase"), "successor runtime credential")
                    .unwrap();
                replacement = Some(successor);
            }
            Change::ExitDuringRead => original.exit(),
            Change::NoPassphrase => {
                std::fs::write(
                    app.join("serve.launch"),
                    serde_json::to_vec(&serde_json::json!({
                        "schema": 1,
                        "pid": original.child.id(),
                        "profile": "default",
                        "host": "127.0.0.1",
                        "port": 8081,
                        "auth_mode": "none",
                        "has_passphrase": false,
                        "behind_proxy": false,
                        "read_only": false,
                        "remote": true,
                        "tunnel_name": null,
                        "tunnel_url": null,
                        "no_tailscale": false
                    }))
                    .unwrap(),
                )
                .unwrap();
            }
        }
        release.send(()).unwrap();
        wait_for(&mut view, |_| next_observation.try_recv().is_ok());
        let ServeViewState::Active {
            passphrase, urls, ..
        } = &view.state
        else {
            panic!("optional credential recovery discarded the Active view");
        };
        assert!(passphrase.is_none());
        assert_eq!(urls[0].url, "https://original.invalid/");
        if let Some(successor) = &mut replacement {
            assert!(successor.child.try_wait().unwrap().is_none());
        }
    }
}

#[test]
#[serial_test::serial]
fn open_rejects_replacement_during_a_metadata_read() {
    use super::work::BEFORE_OBSERVATION_RECHECK;
    let _home = crate::session::test_support::isolate_app_dir();
    let app = crate::session::get_app_dir().unwrap();
    let mut original = TestDaemon::new();
    original.publish();
    std::fs::write(app.join("serve.mode"), "tunnel").unwrap();
    std::fs::write(app.join("serve.passphrase"), "original runtime credential").unwrap();
    std::fs::write(app.join("serve.url"), "https://original.invalid/").unwrap();
    let (entered, received) = mpsc::sync_channel(1);
    let (release, retained) = mpsc::sync_channel(1);
    let mut before_recheck: Option<Box<dyn FnOnce() + Send>> = Some(Box::new(move || {
        entered.send(()).unwrap();
        retained.recv_timeout(Duration::from_secs(10)).unwrap();
    }));
    let mut jobs = ServeJobs::default();
    let submitter = jobs.with_handler(move |job, control, runtime| {
        if matches!(job, Job::Open) {
            BEFORE_OBSERVATION_RECHECK.with(|hook| {
                *hook.borrow_mut() = before_recheck.take();
            });
        }
        run_job(job, control, runtime)
    });
    let mut view = ServeView::with_state(ServeViewState::Checking { attempt: 0 }, submitter);
    view.request(Job::Open, PendingJob::Open);
    view.state = ServeViewState::Checking {
        attempt: view.last_attempt,
    };
    received.recv_timeout(Duration::from_secs(10)).unwrap();
    original.exit();
    let mut replacement = TestDaemon::new();
    replacement.publish();
    std::fs::write(app.join("serve.mode"), "local").unwrap();
    std::fs::write(
        app.join("serve.passphrase"),
        "replacement runtime credential",
    )
    .unwrap();
    std::fs::write(app.join("serve.url"), "https://replacement.invalid/").unwrap();
    release.send(()).unwrap();
    wait_for(&mut view, |view| {
        matches!(
            view.state,
            ServeViewState::Active { .. } | ServeViewState::Error { .. }
        )
    });
    assert!(matches!(view.state, ServeViewState::Error { .. }));
    assert!(replacement.child.try_wait().unwrap().is_none());
}

#[test]
#[serial_test::serial]
fn launch_observations_diagnose_only_a_proven_owner_exit() {
    let _home = crate::session::test_support::isolate_app_dir();
    let app = crate::session::get_app_dir().unwrap();
    #[derive(Clone, Copy)]
    enum Change {
        Exit,
        Hide,
        Foreign,
    }
    for (hold_starting, change, legacy) in [
        (false, Change::Exit, false),
        (true, Change::Exit, false),
        (true, Change::Hide, false),
        (true, Change::Foreign, false),
        (true, Change::Exit, true),
    ] {
        remove_app_file("serve.url");
        let mut daemon = TestDaemon::new();
        let target = daemon.publish();
        let birth = crate::process::process_birth_identity(daemon.child.id())
            .unwrap()
            .unwrap();
        let (entered, received) = mpsc::sync_channel(1);
        let (release, retained) = mpsc::sync_channel(1);
        let mut jobs = ServeJobs::default();
        let submitter = jobs.with_handler(move |job, control, runtime| {
            if operation(&job)
                == if hold_starting {
                    Held::Starting
                } else {
                    Held::LaunchOutcome
                }
            {
                entered.send(()).unwrap();
                retained.recv_timeout(Duration::from_secs(10)).unwrap();
            }
            run_job(job, control, runtime)
        });
        let mut view = ServeView::with_state(starting(), submitter);
        view.launch_in_flight = true;
        view.finish_launch(Ok(target), None);
        if hold_starting {
            wait_for(&mut view, |view| {
                !view
                    .pending
                    .as_ref()
                    .is_some_and(|pending| matches!(pending.kind, PendingJob::LaunchOutcome))
            });
        }
        received.recv_timeout(Duration::from_secs(10)).unwrap();
        if matches!(change, Change::Hide) {
            std::fs::remove_file(app.join("serve.pid")).unwrap();
        } else {
            daemon.exit();
            if matches!(change, Change::Foreign) {
                std::fs::write(app.join("serve.pid"), std::process::id().to_string()).unwrap();
            }
        }
        let identity = if legacy {
            String::new()
        } else {
            format!(" boot_id={} start_id={}", birth.boot_id, birth.start_id)
        };
        std::fs::write(
            crate::cli::serve::stdio_redirect_path().unwrap(),
            format!(
                "[AOE_START_MARKER] pid={}{} exe=aoe\n\
                 {}fixture-own-startup-tail\n\
                 [AOE_START_MARKER] pid={} exe=tui\n\
                 Address already in use\nfixture-late-startup-tail\n\
                 [AOE_START_MARKER] pid={} boot_id={} start_id=departed-reuse exe=aoe\n\
                 unrelated-later-tail\n",
                daemon.child.id(),
                identity,
                "earlier runtime context\n".repeat(32),
                std::process::id(),
                daemon.child.id(),
                birth.boot_id,
            ),
        )
        .unwrap();
        release.send(()).unwrap();
        wait_for(&mut view, |view| {
            matches!(view.state, ServeViewState::Error { .. })
        });
        let ServeViewState::Error { message, .. } = &view.state else {
            unreachable!()
        };
        if matches!(change, Change::Exit) {
            assert!(message.contains("exited before"), "{message}");
            assert!(message.contains("fixture-own-startup-tail"), "{message}");
            assert!(message.contains("Address already in use"), "{message}");
            assert!(message.contains("fixture-late-startup-tail"), "{message}");
            assert!(!message.contains("\n\nHint:"), "{message}");
            if legacy {
                assert!(
                    message.contains("Unattributed shared log context"),
                    "{message}"
                );
            }
            let buffer = crate::tui::dialogs::test_render::draw(100, 30, |frame, theme| {
                view.render(frame, frame.area(), theme);
            });
            crate::tui::dialogs::test_render::find(&buffer, "fixture-late-startup-tail");
        } else {
            assert!(!message.contains("exited before"), "{message}");
            assert!(!message.contains("fixture-own-startup-tail"), "{message}");
            if matches!(change, Change::Hide) {
                assert!(daemon.child.try_wait().unwrap().is_none());
            }
        }
        remove_app_file("serve.pid");
    }
}

#[tokio::test]
#[serial_test::serial]
async fn foreign_log_errno_does_not_invalidate_saved_port_on_launch_failure() {
    let _home = crate::session::test_support::isolate_app_dir();
    let app = crate::session::get_app_dir().unwrap();
    let _startup_exclusion = crate::cli::serve::try_lock_serve()
        .unwrap()
        .expect("isolated startup exclusion");
    let port = app.join("serve.last_port");
    std::fs::write(&port, "55555").unwrap();
    std::fs::write(
        crate::cli::serve::stdio_redirect_path().unwrap(),
        "[AOE_START_MARKER] pid=23 exe=another-client\nAddress already in use\n",
    )
    .unwrap();
    let request = ServeLaunchRequest {
        intent: LaunchIntent::Start(None),
        mode: ServeMode::Local,
        transport: None,
        passphrase: None,
    };
    let target = request.target();
    let failed = tokio::time::timeout(Duration::from_secs(10), request.run(String::new(), None))
        .await
        .unwrap();
    let failure = match failed {
        Err(failure) => failure,
        Ok(_) => panic!("startup exclusion did not refuse the launch"),
    };
    assert!(failure
        .log_tail
        .lines
        .iter()
        .any(|line| line == "Address already in use"));
    assert!(!failure.log_tail.has_process_marker);
    assert_eq!(std::fs::read_to_string(&port).unwrap(), "55555");

    let mut jobs = ServeJobs::default();
    for reopened in [false, true] {
        let mut view = if reopened {
            ServeView::for_launch(
                DashboardLaunch::Failed {
                    target: target.clone(),
                    failure: failure.clone(),
                },
                &mut jobs,
            )
        } else {
            let mut view = ServeView::with_state(starting(), jobs.submitter());
            view.launch_in_flight = true;
            view.finish_launch(Err(failure.clone()), None);
            view
        };
        tokio::time::timeout(Duration::from_secs(10), async {
            while view.pending.is_some() {
                view.tick();
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(matches!(view.state, ServeViewState::Error { .. }));
        assert_eq!(std::fs::read_to_string(&port).unwrap(), "55555");
    }
}

#[test]
#[serial_test::serial]
fn runtime_owned_shutdown_does_not_hold_terminal_teardown_for_an_admitted_reset() {
    let _home = crate::session::test_support::isolate_app_dir();
    for drop_owner in [false, true] {
        let fixture = FunnelFixture::new();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let entered_runtime = runtime.enter();
        let mut jobs = ServeJobs::default();
        let mut view = ServeView::with_state(error_state(), jobs.submitter());
        view.target = Some(crate::cli::serve::tui_control_target().unwrap());
        view.handle_key(key('R'));
        let process = fixture.entered();
        drop(view);
        drop(entered_runtime);
        let (finished, received) = mpsc::sync_channel(1);
        let shutdown = std::thread::spawn(move || {
            if drop_owner {
                drop(jobs);
            } else {
                jobs.shutdown();
            }
            finished.send(()).unwrap();
        });
        let returned = received.recv_timeout(Duration::from_secs(1));
        fixture.release();
        shutdown.join().unwrap();
        drop(runtime);
        assert!(
            returned.is_ok(),
            "terminal teardown waited for the admitted reset"
        );
        assert!(process.exited().unwrap());
        assert_eq!(
            std::fs::read_to_string(fixture.root.path().join("calls")).unwrap(),
            "reset\n"
        );
        assert!(fixture.root.path().join("complete").exists());
    }
}

#[test]
#[serial_test::serial]
fn funnel_reset_rediscovers_repaired_identity_without_replaying_the_mutation() {
    let _home = crate::session::test_support::isolate_app_dir();
    let fixture = FunnelFixture::new();
    let pid = crate::session::get_app_dir().unwrap().join("serve.pid");
    std::fs::create_dir(&pid).unwrap();
    let mut jobs = ServeJobs::default();
    let mut view = ServeView::with_state(error_state(), jobs.submitter());
    view.request(Job::ErrorContext, PendingJob::ErrorContext);
    wait_for(&mut view, |view| view.pending.is_none());
    view.handle_key(key('R'));
    wait_for(&mut view, |view| view.pending.is_none());
    assert!(view.target.is_none());
    assert!(view.control_error.is_some());
    assert!(!fixture.root.path().join("calls").exists());
    std::fs::remove_dir(&pid).unwrap();
    view.handle_key(key('R'));
    wait_for(&mut view, |_| fixture.root.path().join("entered").exists());
    let process = fixture.entered();
    for _ in 0..3 {
        view.handle_key(key('R'));
        view.tick();
    }
    fixture.release();
    wait_for(&mut view, |view| view.pending.is_none());
    assert!(process.exited().unwrap());
    assert_eq!(
        std::fs::read_to_string(fixture.root.path().join("calls")).unwrap(),
        "reset\n"
    );
}

#[test]
#[serial_test::serial]
fn open_refuses_fifo_metadata_without_retaining_lifecycle() {
    use std::os::unix::fs::OpenOptionsExt;
    let _home = crate::session::test_support::isolate_app_dir();
    let app = crate::session::get_app_dir().unwrap();
    let mut daemon = TestDaemon::new();
    std::fs::write(app.join("serve.mode"), "local").unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    for (name, retain_writer) in [
        ("serve.url", false),
        ("serve.url", true),
        ("serve.pid", false),
        ("serve.pid", true),
    ] {
        daemon.publish();
        let path = app.join(name);
        if name == "serve.pid" {
            std::fs::remove_file(&path).unwrap();
        }
        nix::unistd::mkfifo(
            &path,
            nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
        )
        .unwrap();
        let open_gate = || {
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(nix::libc::O_NONBLOCK)
                .open(&path)
                .unwrap()
        };
        let writer = retain_writer.then(open_gate);
        let mut jobs = ServeJobs::default();
        let mut view = ServeView::new(&mut jobs);
        let deadline = Instant::now() + Duration::from_secs(10);
        let settled = loop {
            view.tick();
            if !matches!(view.state, ServeViewState::Checking { .. }) {
                break if name == "serve.pid" {
                    matches!(view.state, ServeViewState::Error { .. })
                } else {
                    matches!(view.state, ServeViewState::Starting { .. })
                };
            }
            if Instant::now() >= deadline {
                break false;
            }
            std::thread::yield_now();
        };
        drop(view);
        let unlocked = runtime
            .block_on(async {
                tokio::time::timeout(
                    Duration::from_secs(10),
                    crate::daemon::lifecycle::Transaction::acquire(),
                )
                .await
            })
            .is_ok_and(|transaction| transaction.is_ok());
        let writer = writer.unwrap_or_else(open_gate);
        drop(writer);
        drop(jobs);
        std::fs::remove_file(path).unwrap();
        assert!(
            settled,
            "FIFO retained Open; file={name}; writer={retain_writer}"
        );
        assert!(
            unlocked,
            "FIFO retained lifecycle exclusion; file={name}; writer={retain_writer}"
        );
    }
}

#[test]
#[serial_test::serial]
fn picker_reobserves_dashboard_after_provider_probes() {
    let _home = crate::session::test_support::isolate_app_dir();
    let app = crate::session::get_app_dir().unwrap();
    for reopen_after_stop in [false, true] {
        remove_app_file("serve.pid");
        remove_app_file("serve.url");
        let fixture = FunnelFixture::new();
        std::fs::write(fixture.root.path().join("hold-probe"), "").unwrap();
        let mut jobs = ServeJobs::default();
        let mut view = if reopen_after_stop {
            let mut view = ServeView::with_state(error_state(), jobs.submitter());
            view.begin_stop();
            view.finish_stop(Ok(()));
            view
        } else {
            ServeView::new(&mut jobs)
        };
        let probe = fixture.entered();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let transaction = runtime
            .block_on(crate::daemon::lifecycle::Transaction::acquire())
            .unwrap();
        let mut daemon = TestDaemon::new();
        let target = daemon.publish();
        std::fs::write(app.join("serve.mode"), "local").unwrap();
        std::fs::write(app.join("serve.url"), "http://127.0.0.1:23456/").unwrap();
        drop(transaction);
        fixture.release();
        wait_for(&mut view, |view| {
            !matches!(view.state, ServeViewState::Checking { .. })
        });
        assert!(
            matches!(&view.state, ServeViewState::Active { urls, .. }
                if urls[0].url == "http://127.0.0.1:23456/"),
            "picker missed dashboard; after_stop={reopen_after_stop}"
        );
        assert_eq!(view.target.as_ref(), Some(&target));
        assert!(view.pending_launch.is_none());
        assert!(probe.exited().unwrap());
    }
}

#[tokio::test]
#[serial_test::serial]
async fn stop_retains_the_original_retry_completion() {
    let _home = crate::session::test_support::isolate_app_dir();
    let _tools = FunnelFixture::new();
    for outcome in ["stopped", "refused", "worker-closed"] {
        let mut original = TestDaemon::new();
        let target = original.publish();
        let mut successor = (outcome == "refused").then(|| {
            original.exit();
            let mut successor = TestDaemon::new();
            successor.publish();
            successor
        });
        let (entered, ready) = mpsc::sync_channel(1);
        let (release, resume) = mpsc::sync_channel(1);
        let mut jobs = ServeJobs::default();
        let submitter = jobs.with_handler(move |job, control, runtime| {
            let retry = matches!(job, Job::Retry(_));
            let result = run_job(job, control, runtime);
            if retry {
                entered.send(()).unwrap();
                resume.recv_timeout(Duration::from_secs(10)).unwrap();
                assert!(
                    outcome != "worker-closed",
                    "controlled worker closure after stop"
                );
            }
            result
        });
        let mut view = ServeView::with_state(error_state(), submitter);
        view.target = Some(target);
        view.handle_key(key('P'));
        ready.recv_timeout(Duration::from_secs(10)).unwrap();
        assert!(original.child.try_wait().unwrap().is_some());
        let ServeAction::Stop(request) = view.handle_key(key('S')) else {
            panic!("S must retain its provider stop");
        };
        let owner = tokio::spawn(request.run(&mut jobs));
        assert!(matches!(
            view.handle_key(KeyEvent::from(KeyCode::Esc)),
            ServeAction::Close
        ));
        drop(view);
        let mut reopened = ServeView::new(&mut jobs);
        release.send(()).unwrap();
        let stopped = tokio::time::timeout(Duration::from_secs(10), owner)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            stopped.is_ok(),
            outcome == "stopped",
            "{outcome}: {stopped:?}"
        );
        wait_for(&mut reopened, |view| {
            !matches!(view.state, ServeViewState::Checking { .. })
        });
        if let Some(successor) = &mut successor {
            assert!(successor.child.try_wait().unwrap().is_none());
        }
        assert!(reopened.pending_launch.is_none());
        remove_app_file("serve.pid");
    }
}

#[tokio::test]
#[serial_test::serial]
async fn restart_preserves_a_replacement_before_any_persisted_effect() {
    let _home = crate::session::test_support::isolate_app_dir();
    let tools = FunnelFixture::new();
    for code in ['R', 'G'] {
        for replacement in ["before-confirmation", "during-assessment", "after-handoff"] {
            let mut original = TestDaemon::new();
            let target = original.publish();
            let mut successor = TestDaemon::new();
            let mut jobs = ServeJobs::default();
            let (submitter, gate) = gated(&mut jobs, Held::Restart);
            let mut state = active_state(ServeMode::Tunnel);
            if let ServeViewState::Active { transport, .. } = &mut state {
                *transport = Some(TunnelTransport::Tailscale);
            }
            let mut view = ServeView::with_state(state, submitter);
            view.target = Some(target);
            view.handle_key(key(code));
            gate.entered(Held::Restart);
            gate.release(JobResult::Restart(RestartProbe::default()));
            wait_for(&mut view, |view| view.pending_confirm.is_some());
            if replacement == "before-confirmation" {
                original.exit();
                successor.publish();
            }
            view.handle_key(key(code));
            gate.entered(Held::Restart);
            if replacement == "during-assessment" {
                original.exit();
                successor.publish();
            }
            gate.release(JobResult::Restart(RestartProbe::default()));
            wait_assessment(&mut view);
            let request = view
                .take_launch()
                .expect("confirmed restart must reach its owner");
            let transaction = crate::daemon::lifecycle::Transaction::acquire()
                .await
                .unwrap();
            if replacement == "after-handoff" {
                original.exit();
                successor.publish();
            }
            remove_app_file("serve.last_port");
            save_passphrase_to_disk("fixture future default");
            let app = crate::session::get_app_dir().unwrap();
            let metadata = || {
                std::fs::read_dir(&app)
                    .unwrap()
                    .map(|entry| entry.unwrap().path())
                    .filter(|path| path.is_file())
                    .map(|path| {
                        let contents = std::fs::read(&path).unwrap();
                        (path, contents)
                    })
                    .collect::<std::collections::BTreeMap<_, _>>()
            };
            let context = request.target();
            if replacement == "during-assessment" {
                drop(view);
                view = ServeView::for_launch(
                    DashboardLaunch::Running {
                        target: context.clone(),
                        started_at: Instant::now(),
                    },
                    &mut jobs,
                );
            }
            let before = metadata();
            let mut launch = std::pin::pin!(request.run("test".to_owned(), None));
            std::future::poll_fn(|context| {
                assert!(std::future::Future::poll(launch.as_mut(), context).is_pending());
                std::task::Poll::Ready(())
            })
            .await;
            assert_eq!(
                metadata(),
                before,
                "{code}/{replacement}: effects preceded admission"
            );
            drop(transaction);
            let failure = match tokio::time::timeout(Duration::from_secs(10), launch)
                .await
                .unwrap()
            {
                Err(failure) => failure,
                Ok(_) => panic!("stale restart unexpectedly succeeded"),
            };
            if replacement == "after-handoff" {
                drop(view);
                view = ServeView::for_launch(
                    DashboardLaunch::Failed {
                        target: context,
                        failure,
                    },
                    &mut jobs,
                );
            } else {
                view.finish_launch(Err(failure), context.observed_target);
            }
            wait_for(&mut view, |view| view.pending.is_none());
            let recovery = match replacement {
                "before-confirmation" => 'P',
                "during-assessment" => 'S',
                _ => 'R',
            };
            match view.handle_key(key(recovery)) {
                ServeAction::Stop(request) => {
                    assert!(request.run(&mut jobs).await.is_err());
                }
                ServeAction::Continue => {
                    assert!(view.pending.as_ref().is_some_and(|pending| matches!(
                        pending.kind,
                        PendingJob::Retry { .. } | PendingJob::Reset
                    )));
                    wait_for(&mut view, |view| view.pending.is_none());
                }
                _ => panic!("recovery did not reach its control owner"),
            }
            assert!(!tools.root.path().join("calls").exists());
            assert!(successor.child.try_wait().unwrap().is_none());
            assert_eq!(
                metadata(),
                before,
                "{code}/{replacement}: refusal changed persisted state"
            );
            remove_app_file("serve.pid");
        }
    }
}

#[cfg(target_os = "linux")]
#[test]
#[serial_test::serial]
fn quitting_retains_an_admitted_direct_stop_until_physical_retirement() {
    let _home = crate::session::test_support::isolate_app_dir();
    let _tools = FunnelFixture::new();
    let mut daemon = TestDaemon::new();
    let target = daemon.publish();
    let pid = daemon.child.id();
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pid as i32),
        nix::sys::signal::Signal::SIGSTOP,
    )
    .unwrap();
    let status_path = format!("/proc/{pid}/status");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !std::fs::read_to_string(&status_path)
        .unwrap()
        .lines()
        .any(|line| line.starts_with("State:") && line.split_whitespace().nth(1) == Some("T"))
    {
        assert!(Instant::now() < deadline, "fixture Daemon did not stop");
        std::thread::yield_now();
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let mut jobs = ServeJobs::default();
        let mut view = ServeView::with_state(active_state(ServeMode::Tunnel), jobs.submitter());
        view.target = Some(target);
        let ServeAction::Stop(request) = view.handle_key(key('S')) else {
            panic!("S did not request a direct stop");
        };
        tokio::spawn(request.run(&mut jobs));
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let status = std::fs::read_to_string(&status_path).unwrap();
            let pending =
                status
                    .lines()
                    .filter_map(|line| line.split_once(':'))
                    .any(|(name, value)| {
                        matches!(name, "SigPnd" | "ShdPnd")
                            && u64::from_str_radix(value.trim(), 16).unwrap()
                                & (1 << (nix::libc::SIGTERM - 1))
                                != 0
                    });
            if pending {
                break;
            }
            assert!(Instant::now() < deadline, "Stop never sent SIGTERM");
            tokio::task::yield_now().await;
        }
        drop(view);
        jobs.shutdown();
    });
    drop(runtime);
    assert!(daemon.child.try_wait().unwrap().is_some());
    assert!(!crate::session::get_app_dir()
        .unwrap()
        .join("serve.pid")
        .exists());
}

#[tokio::test]
#[serial_test::serial]
async fn launch_control_preserves_observed_absence_after_fresh_start_refusal() {
    let _home = crate::session::test_support::isolate_app_dir();
    let tools = FunnelFixture::new();
    let app = crate::session::get_app_dir().unwrap();
    std::fs::create_dir_all(app.join("profiles/default")).unwrap();
    for recovery in ['P', 'S', 'R'] {
        let mut jobs = ServeJobs::default();
        let mut view = ServeView::settled(&mut jobs);
        assert!(matches!(view.state, ServeViewState::ModePicker { .. }));
        view.handle_key(key('T'));
        wait_for(&mut view, |view| {
            matches!(view.state, ServeViewState::Confirm { .. })
        });
        view.handle_key(KeyEvent::from(KeyCode::Left));
        assert!(matches!(
            view.state,
            ServeViewState::Confirm {
                selected: TunnelTransport::Tailscale,
                ..
            }
        ));
        view.handle_key(KeyEvent::from(KeyCode::Enter));
        wait_assessment(&mut view);
        let request = view
            .take_launch()
            .expect("provider confirmation did not hand off startup");
        let context = request.target();
        if recovery == 'S' {
            drop(view);
            view = ServeView::for_launch(
                DashboardLaunch::Running {
                    target: context.clone(),
                    started_at: Instant::now(),
                },
                &mut jobs,
            );
        }
        let mut successor = TestDaemon::new();
        std::fs::write(app.join("serve.mode"), "tunnel").unwrap();
        std::fs::write(app.join("serve.url"), "https://fixture.tailnet.ts.net/").unwrap();
        std::fs::write(app.join("serve.passphrase"), "external fixture credential").unwrap();
        successor.publish();
        let published = ["serve.pid", "serve.mode", "serve.url", "serve.passphrase"]
            .map(|name| (name, std::fs::read(app.join(name)).unwrap()));
        let failure = match request.run("default".to_owned(), None).await {
            Err(failure) => failure,
            Ok(_) => panic!("fresh start unexpectedly replaced the external dashboard"),
        };
        if recovery == 'R' {
            drop(view);
            view = ServeView::for_launch(
                DashboardLaunch::Failed {
                    target: context,
                    failure,
                },
                &mut jobs,
            );
        } else {
            view.finish_launch(Err(failure), context.observed_target);
        }
        wait_for(&mut view, |view| view.pending.is_none());
        match view.handle_key(key(recovery)) {
            ServeAction::Stop(request) => {
                assert!(request.run(&mut jobs).await.is_err());
            }
            ServeAction::Continue => {
                assert!(view.pending.as_ref().is_some_and(|pending| matches!(
                    pending.kind,
                    PendingJob::Retry { .. } | PendingJob::Reset
                )));
                wait_for(&mut view, |view| view.pending.is_none());
            }
            _ => panic!("fresh-start recovery did not reach its control owner"),
        }
        assert!(successor.child.try_wait().unwrap().is_none());
        assert!(!tools.root.path().join("calls").exists());
        for (name, expected) in published {
            assert_eq!(std::fs::read(app.join(name)).unwrap(), expected);
        }
        successor.exit();
        for name in ["serve.pid", "serve.mode", "serve.url", "serve.passphrase"] {
            remove_app_file(name);
        }
    }
}
