use super::*;
use std::sync::mpsc;

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
    Reset,
    Restart,
    LaunchContext,
    LaunchOutcome,
    Starting,
    Passphrase,
    ErrorContext,
    ActiveLog,
    Copy,
}

fn operation(job: &Job) -> Held {
    match job {
        Job::Open => Held::Open,
        Job::Picker => Held::Picker,
        Job::Providers => Held::Providers,
        Job::Retry(_) => Held::Retry,
        Job::Reset(_) => Held::Reset,
        Job::Restart(..) => Held::Restart,
        Job::LaunchContext { .. } => Held::LaunchContext,
        Job::LaunchOutcome { .. } => Held::LaunchOutcome,
        Job::ObserveStarting => Held::Starting,
        Job::Passphrase => Held::Passphrase,
        Job::ErrorContext => Held::ErrorContext,
        Job::ObserveActive => Held::ActiveLog,
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
            Job::ObserveActive => JobResult::ActiveLog(0),
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
fn new_passphrase_is_saved_only_after_the_restart_succeeds() {
    let _home = crate::session::test_support::isolate_app_dir();
    save_passphrase_to_disk("running daemon phrase");
    let mut jobs = ServeJobs::default();
    let mut view = ServeView::with_state(active_state(ServeMode::Tunnel), jobs.submitter());
    view.handle_key(key('G'));
    wait_assessment(&mut view);
    view.handle_key(key('G'));
    wait_assessment(&mut view);
    assert!(view.take_launch().is_some_and(|request| request.restart));
    assert_ne!(view.pending_passphrase, "running daemon phrase");
    assert_eq!(
        recall_passphrase().as_deref(),
        Some("running daemon phrase")
    );
}

#[test]
#[serial_test::serial]
fn restart_requires_an_uninterrupted_confirmation() {
    let _home = crate::session::test_support::isolate_app_dir();
    for (cancelled, expired) in [(false, false), (true, false), (false, true)] {
        let mut jobs = ServeJobs::default();
        let mut view = ServeView::with_state(active_state(ServeMode::Local), jobs.submitter());
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
            assert!(view
                .take_launch()
                .is_some_and(|request| request.restart && request.mode == ServeMode::Local));
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
    assert!(view.take_launch().is_some_and(|request| request.restart));
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
        let (exercise, exercise_rx) = mpsc::sync_channel(1);
        let (finished, finished_rx) = mpsc::sync_channel(1);
        let ui = std::thread::spawn(move || {
            match held {
                Held::Open => view.request(Job::Open, PendingJob::Open),
                Held::Picker => {
                    view.begin_stop();
                    view.finish_stop(Ok(()));
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
            let (col, row) = crate::tui::dialogs::test_render::find(&buffer, "[Esc] cancel");
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
        gate.release(JobResult::Failed("late old failure".to_owned()));
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
fn every_pending_state_leaves_a_closed_worker() {
    let _home = crate::session::test_support::isolate_app_dir();
    for held in [
        Held::Open,
        Held::Picker,
        Held::Providers,
        Held::Retry,
        Held::Reset,
        Held::Restart,
        Held::LaunchContext,
        Held::LaunchOutcome,
        Held::Starting,
        Held::Passphrase,
        Held::ErrorContext,
        Held::ActiveLog,
        Held::Copy,
    ] {
        let mut view = ServeView::for_test(if held == Held::ErrorContext {
            error_state()
        } else {
            active_state(ServeMode::Tunnel)
        });
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
            Held::Retry => (Job::Retry(target), PendingJob::Retry { failed: None }),
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
                    restart: false,
                    mode: ServeMode::Local,
                    transport: None,
                    passphrase: None,
                }),
            ),
            Held::LaunchOutcome => (
                Job::LaunchOutcome {
                    mode: ServeMode::Local,
                    transport: None,
                    passphrase: None,
                    log_offset: 0,
                },
                PendingJob::LaunchOutcome,
            ),
            Held::Starting => (Job::ObserveStarting, PendingJob::Starting),
            Held::Passphrase => (Job::Passphrase, PendingJob::Passphrase),
            Held::ErrorContext => (Job::ErrorContext, PendingJob::ErrorContext),
            Held::ActiveLog => (Job::ObserveActive, PendingJob::ActiveLog),
            Held::Copy => (Job::Copy("URL", "fixture".to_owned()), PendingJob::Copy),
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
            tail: Vec::new(),
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
        assert!(matches!(view.handle_key(key('S')), ServeAction::Stop));
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn stop_key_returns_owned_action_while_launch_transaction_is_held() {
    let _home = crate::session::test_support::isolate_app_dir();
    let transaction = crate::daemon::lifecycle::Transaction::acquire()
        .await
        .unwrap();
    let mut view = ServeView::for_test(starting());
    view.launch_in_flight = true;
    let (sent, received) = tokio::sync::oneshot::channel();
    let ui = std::thread::spawn(move || {
        let action = view.handle_key(key('S'));
        assert!(sent.send((view, action)).is_ok());
    });
    let (mut view, action) = tokio::time::timeout(Duration::from_secs(2), received)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(action, ServeAction::Stop));
    let buffer = crate::tui::dialogs::test_render::draw(110, 40, |frame, theme| {
        view.render(frame, frame.area(), theme)
    });
    crate::tui::dialogs::test_render::find(&buffer, "Stopping daemon");
    assert!(matches!(
        view.handle_key(KeyEvent::from(KeyCode::Esc)),
        ServeAction::Close
    ));
    drop(transaction);
    ui.join().unwrap();
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
      sleep 600 &
      printf '%s\n' "$!" > "$AOE_TEST_FUNNEL_ROOT/descendant"
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
        nix::unistd::mkfifo(
            &root.path().join("release"),
            nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
        )
        .unwrap();
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
