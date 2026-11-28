//! The sandbox login action: palette listing, dialog routing and the events from
//! the login thread.

use super::*;
use crate::session::config::SharedCredentialMode;
use crate::session::sandbox_login::helper::LoginError;
use crate::tui::dialogs::SandboxLoginDialog;
use crate::tui::home::bindings::ActionId;
use crate::tui::sandbox_login_poller::{LoginEvent, SandboxLoginPoller};
use ratatui::backend::TestBackend;
use ratatui::Terminal;

const TITLE: &str = "Log the sandboxes in";

fn screen(env: &mut TestEnv) -> String {
    let theme = crate::tui::styles::load_theme("empire");
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
    terminal
        .draw(|f| {
            let area = f.area();
            env.view.render(f, area, &theme, None);
        })
        .unwrap();
    let buf = terminal.backend().buffer().clone();
    (0..buf.area.height)
        .map(|y| {
            (0..buf.area.width)
                .map(|x| buf[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn open_palette_for(env: &mut TestEnv, query: &str) {
    env.view.handle_key(
        KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL),
        None,
    );
    for ch in query.chars() {
        env.view.handle_key(key(KeyCode::Char(ch)), None);
    }
}

#[test]
#[serial]
fn sandbox_login_palette_entry_is_listed_only_under_separate_login() {
    let home = TempDir::new().unwrap();
    let _home = crate::session::test_support::isolate_home(home.path());
    let mut env = create_test_env_with_sessions(1);

    open_palette_for(&mut env, "log the sandboxes");
    assert!(!screen(&mut env).contains(TITLE));
    env.view.handle_key(key(KeyCode::Esc), None);

    crate::session::config::update_config(|c| {
        c.sandbox.shared_credential = SharedCredentialMode::SeparateLogin;
    })
    .unwrap();
    open_palette_for(&mut env, "log the sandboxes");
    assert!(screen(&mut env).contains(TITLE));
}

#[test]
#[serial]
fn sandbox_login_action_shows_why_it_cannot_start_and_closes_on_esc() {
    let home = TempDir::new().unwrap();
    let _home = crate::session::test_support::isolate_home(home.path());
    let mut env = create_test_env_with_sessions(1);

    env.view.run_action(ActionId::SandboxLogin, None);
    assert!(env.view.sandbox_login_dialog.is_some());
    assert!(screen(&mut env).contains("Starting the login"));

    // host_seed mode: the helper refuses at once, and the dialog says why.
    let event = env
        .view
        .sandbox_login_poller
        .as_ref()
        .unwrap()
        .recv_for_test();
    env.view.apply_sandbox_login_event(event);
    assert!(screen(&mut env).contains("separate_login"));

    // Starting again cannot help while the setting is off, so Enter does nothing.
    env.view.handle_key(key(KeyCode::Enter), None);
    assert!(screen(&mut env).contains("separate_login"));

    env.view.handle_key(key(KeyCode::Esc), None);
    assert!(env.view.sandbox_login_dialog.is_none());
    assert!(env.view.sandbox_login_poller.is_none());
}

/// Apple Container cannot run the helper: the action shows the setting's refusal instead
/// of starting one, and offers no retry that could not help.
#[test]
#[serial]
fn sandbox_login_action_shows_the_apple_container_refusal() {
    use crate::tui::sandbox_login_poller::StartFailure;

    let mut env = create_test_env_with_sessions(1);
    crate::session::config::update_config(|c| {
        c.sandbox.shared_credential = SharedCredentialMode::SeparateLogin;
        c.sandbox.container_runtime = crate::session::config::ContainerRuntimeName::AppleContainer;
    })
    .unwrap();

    env.view.run_action(ActionId::SandboxLogin, None);
    let event = env
        .view
        .sandbox_login_poller
        .as_ref()
        .unwrap()
        .recv_for_test();
    let reason = crate::session::SandboxLoginUnsupported.to_string();
    match &event {
        LoginEvent::StartFailed { message, failure } => {
            assert_eq!(message, &reason);
            assert_eq!(*failure, StartFailure::Unsupported);
        }
        _ => panic!("the refusal is a start failure"),
    }
    env.view.apply_sandbox_login_event(event);
    let shown = screen(&mut env);
    assert!(shown.contains("Apple Container"), "{shown}");
    assert!(!shown.contains("try again"), "{shown}");

    // Enter has nothing to retry; Esc closes.
    env.view.handle_key(key(KeyCode::Enter), None);
    assert!(env.view.sandbox_login_dialog.is_some());
    assert!(screen(&mut env).contains("Apple Container"));
    env.view.handle_key(key(KeyCode::Esc), None);
    assert!(env.view.sandbox_login_dialog.is_none());
}

#[test]
#[serial]
fn sandbox_login_success_closes_the_dialog_and_says_so() {
    let mut env = create_test_env_with_sessions(1);
    env.view.sandbox_login_dialog = Some(SandboxLoginDialog::new());
    env.view.apply_sandbox_login_event(LoginEvent::LoggedIn);
    assert!(env.view.sandbox_login_dialog.is_none());
    assert!(env.view.sandbox_login_poller.is_none());
    let shown = screen(&mut env);
    assert!(shown.contains("Logged in"), "{shown}");
}

#[test]
#[serial]
fn sandbox_login_dialog_takes_paste_and_the_code_stays_masked() {
    let mut env = create_test_env_with_sessions(1);
    let mut dialog = SandboxLoginDialog::new();
    dialog.set_ready("https://example.test/login".to_string());
    env.view.sandbox_login_dialog = Some(dialog);
    assert!(env.view.has_dialog());

    env.view.handle_paste("abc#secret");
    let shown = screen(&mut env);
    assert!(shown.contains("••••••••••"), "{shown}");
    assert!(!shown.contains("abc#secret"), "{shown}");
    assert!(
        env.view.send_message_dialog.is_none(),
        "a paste must not open the message dialog behind the login"
    );

    env.view.handle_key(key(KeyCode::Esc), None);
    assert!(env.view.sandbox_login_dialog.is_none());
}

fn ready_dialog() -> SandboxLoginDialog {
    let mut dialog = SandboxLoginDialog::new();
    dialog.set_ready("https://example.test/login?state=SAMPLEstate".to_string());
    dialog
}

#[test]
#[serial]
fn sandbox_login_tick_moves_the_thread_events_into_the_dialog() {
    let mut env = create_test_env_with_sessions(1);
    env.view.sandbox_login_dialog = Some(SandboxLoginDialog::new());
    let poller = SandboxLoginPoller::spawn_with(async { Err(LoginError::Busy) });
    poller.wait_done_for_test();
    env.view.sandbox_login_poller = Some(poller);

    assert!(env.view.tick_sandbox_login());
    let shown = screen(&mut env);
    // The final event is shown, not replaced by the thread having ended.
    assert!(shown.contains(&LoginError::Busy.to_string()), "{shown}");
    assert!(shown.contains("try again"), "{shown}");
    assert!(!env.view.tick_sandbox_login());
}

#[test]
#[serial]
fn sandbox_login_a_dead_thread_does_not_leave_the_dialog_stuck() {
    let mut env = create_test_env_with_sessions(1);
    env.view.sandbox_login_dialog = Some(ready_dialog());
    env.view.sandbox_login_poller = Some(SandboxLoginPoller::spawn_with(async {
        panic!("login thread died")
    }));
    for c in "abc".chars() {
        env.view.handle_key(key(KeyCode::Char(c)), None);
    }
    env.view.handle_key(key(KeyCode::Enter), None);
    assert!(screen(&mut env).contains("Checking the code"));
    // While the code is in flight Esc does nothing.
    env.view.handle_key(key(KeyCode::Esc), None);
    assert!(env.view.sandbox_login_dialog.is_some());

    env.view
        .sandbox_login_poller
        .as_ref()
        .unwrap()
        .wait_done_for_test();
    assert!(env.view.tick_sandbox_login());
    let shown = screen(&mut env);
    assert!(shown.contains("The login worker stopped"), "{shown}");
    assert!(env.view.sandbox_login_poller.is_none());
    env.view.handle_key(key(KeyCode::Esc), None);
    assert!(env.view.sandbox_login_dialog.is_none());
}

#[test]
#[serial]
fn sandbox_login_esc_while_starting_does_not_block_the_ui() {
    let mut env = create_test_env_with_sessions(1);
    env.view.sandbox_login_dialog = Some(SandboxLoginDialog::new());
    let (release, released) = tokio::sync::oneshot::channel::<()>();
    env.view.sandbox_login_poller = Some(SandboxLoginPoller::spawn_with(async move {
        let _ = released.await;
        Err(LoginError::Gone)
    }));
    let began = std::time::Instant::now();
    env.view.handle_key(key(KeyCode::Esc), None);
    assert!(began.elapsed() < std::time::Duration::from_millis(500));
    assert!(env.view.sandbox_login_dialog.is_none());
    let _ = release.send(());
}

#[test]
#[serial]
fn sandbox_login_a_quit_after_closing_the_dialog_still_waits_for_the_removal() {
    use crate::session::sandbox_login::claude::LOGIN;
    use crate::session::sandbox_login::helper::{HelperTimeouts, LoginHelper};
    use crate::session::sandbox_login::test_fake::Fake;

    let fake = Fake::new();
    // A removal that runs until the test lets it go, as a slow real one does.
    let gate = fake.tmp.path().join("gate");
    let cleanup = Some(vec![
        "sh".to_string(),
        "-c".to_string(),
        format!(
            "while [ ! -e '{}' ]; do sleep 0.05; done; touch '{}'",
            gate.display(),
            fake.cleaned().display()
        ),
    ]);
    let (argv, shared) = (fake.argv(true), fake.shared.clone());
    let mut env = create_test_env_with_sessions(1);
    env.view.sandbox_login_dialog = Some(SandboxLoginDialog::new());
    let poller = SandboxLoginPoller::spawn_with(async move {
        LoginHelper::start_with(&LOGIN, argv, shared, cleanup, HelperTimeouts::default()).await
    });
    assert!(matches!(poller.recv_for_test(), LoginEvent::Started { .. }));
    env.view.sandbox_login_poller = Some(poller);

    env.view.handle_key(key(KeyCode::Esc), None);
    assert!(env.view.sandbox_login_dialog.is_none());
    assert!(!fake.cleaned().exists(), "the removal is still running");

    // Quit: the view goes, and with it the wait for the helper's cleanup, which is let go
    // while the quit waits.
    let release = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(300));
        std::fs::write(gate, "").unwrap();
    });
    drop(env);
    release.join().unwrap();
    assert!(fake.cleaned().exists(), "quit waited for the removal");
}

#[test]
#[serial]
fn sandbox_login_dialog_stays_out_of_the_paste_burst_path_and_logs_nothing() {
    let capture = crate::session::test_support::LogCapture::start();
    let mut env = create_test_env_with_sessions(1);
    assert!(env.view.wants_paste_burst());
    env.view.sandbox_login_dialog = Some(ready_dialog());
    // The burst path logs the pasted text, so the dialog must not ask for it.
    assert!(!env.view.wants_paste_burst());

    env.view.handle_paste("pasted#SECRETcode");
    env.view.handle_key(key(KeyCode::Enter), None);
    let logs = capture.contents();
    assert!(!logs.contains("SECRETcode"), "{logs}");
}

#[test]
#[serial]
fn sandbox_login_link_actions_never_flash_the_link() {
    let mut env = create_test_env_with_sessions(1);
    env.view.sandbox_login_dialog = Some(ready_dialog());
    let url = env
        .view
        .sandbox_login_dialog
        .as_ref()
        .unwrap()
        .url()
        .unwrap()
        .to_string();

    let opened = std::cell::Cell::new(false);
    env.view.open_sign_in_link(
        &url,
        |_| {
            opened.set(true);
            Ok(())
        },
        |_| panic!("an opened link is not copied"),
    );
    assert!(opened.get());
    let text = env.view.status_flash_text().unwrap().to_string();
    assert_eq!(text, "opened the sign-in link");

    let copied = std::cell::RefCell::new(String::new());
    env.view.open_sign_in_link(
        &url,
        |_| Err(std::io::Error::other("no browser you could see from here")),
        |u| {
            *copied.borrow_mut() = u.to_string();
            "Copy queued; waiting for clipboard output.".into()
        },
    );
    assert_eq!(*copied.borrow(), url);
    let text = env.view.status_flash_text().unwrap().to_string();
    assert_eq!(
        text,
        "no browser here; Copy queued; waiting for clipboard output."
    );
    assert!(!text.contains("state="));
}

#[test]
#[serial]
fn sandbox_login_palette_does_not_wait_for_the_credential_lock() {
    use fs2::FileExt;
    let home = TempDir::new().unwrap();
    let _home = crate::session::test_support::isolate_home(home.path());
    let mut env = create_test_env_with_sessions(1);
    crate::session::config::update_config(|c| {
        c.sandbox.shared_credential = SharedCredentialMode::SeparateLogin;
    })
    .unwrap();
    let profile = env.view.config_profile();
    let shared = crate::session::config::container_config::login_target(&profile, "claude")
        .unwrap()
        .credential;
    std::fs::create_dir_all(shared.parent().unwrap()).unwrap();
    // Without the file a status read returns before it ever takes the lock.
    std::fs::write(&shared, "{}").unwrap();
    let lock = std::fs::File::create(shared.with_file_name(".credentials.lock")).unwrap();
    lock.lock_exclusive().unwrap();
    // A sandbox come-up holds the lock for a moment; the UI must not wait for it.
    let release = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(3));
        drop(lock);
    });

    let began = std::time::Instant::now();
    open_palette_for(&mut env, "log the sandboxes");
    let waited = began.elapsed();
    release.join().unwrap();
    assert!(
        waited < std::time::Duration::from_secs(1),
        "waited {waited:?}"
    );
    assert!(screen(&mut env).contains(TITLE));
}

#[test]
#[serial]
fn sandbox_login_runs_for_the_selected_sessions_tool() {
    use crate::tui::sandbox_login_poller::StartFailure;

    // The env isolates its own home, so the config and files go in after it.
    let mut env = create_test_env_with_sessions(2);
    let home = dirs::home_dir().unwrap();
    let (_runtime, _calls) = crate::session::test_support::failing_container_runtime(&home);
    crate::session::config::update_config(|c| {
        c.sandbox.shared_credential = SharedCredentialMode::SeparateLogin;
        c.session
            .custom_agents
            .insert("claude-personal".to_string(), "claude".to_string());
        c.session
            .agent_detect_as
            .insert("claude-personal".to_string(), "claude".to_string());
        c.session.agent_config_dir.insert(
            "claude-personal".to_string(),
            "~/.claude-personal".to_string(),
        );
    })
    .unwrap();
    let ids: Vec<String> = env.view.instances.keys().cloned().collect();
    env.view.instances.get_mut(&ids[0]).unwrap().tool = "claude-personal".to_string();
    env.view.instances.get_mut(&ids[1]).unwrap().tool = "codex".to_string();

    env.view.selected_session = None;
    assert_eq!(
        env.view.sandbox_login_target().map(|t| t.tool).as_deref(),
        Some("claude"),
        "nothing selected: the profile's default login tool"
    );
    env.view.selected_session = Some(ids[0].clone());
    assert_eq!(
        env.view.sandbox_login_target().map(|t| t.tool).as_deref(),
        Some("claude-personal")
    );
    env.view.selected_session = Some(ids[1].clone());
    assert_eq!(
        env.view.sandbox_login_target().map(|t| t.tool).as_deref(),
        Some("claude"),
        "an agent with no sandbox login falls back to the default"
    );

    // The helper reads the wrapper's file, which already holds a login.
    let wrapper = home.join(".claude-personal/sandbox-v2/.credentials.json");
    std::fs::create_dir_all(wrapper.parent().unwrap()).unwrap();
    let expires_at = crate::time::Timestamp::now().as_millis() + 3_600_000;
    std::fs::write(
        &wrapper,
        format!(
            r#"{{"claudeAiOauth":{{"accessToken":"fake-access","refreshToken":"fake-refresh","expiresAt":{expires_at}}}}}"#
        ),
    )
    .unwrap();
    env.view.selected_session = Some(ids[0].clone());
    env.view.run_action(ActionId::SandboxLogin, None);
    let event = env
        .view
        .sandbox_login_poller
        .as_ref()
        .unwrap()
        .recv_for_test();
    assert!(
        matches!(
            event,
            LoginEvent::StartFailed {
                failure: StartFailure::AlreadyOk,
                ..
            }
        ),
        "the selected wrapper session's login was started, not plain claude's"
    );
}

/// A session with its own command for its tool waits on that command's file, which the
/// profile's command for the tool does not reach: the action resolves the target the launch
/// gate resolves (the session's profile, tool and command) and says which command line logs
/// in the same file.
#[test]
#[serial]
fn sandbox_login_runs_for_the_selected_sessions_command() {
    use crate::tui::home::sandbox_login::LoginSelection;
    use crate::tui::sandbox_login_poller::StartFailure;

    let mut env = create_test_env_with_sessions(2);
    let home = dirs::home_dir().unwrap();
    let (_runtime, _calls) = crate::session::test_support::failing_container_runtime(&home);
    crate::session::config::update_config(|c| {
        c.sandbox.shared_credential = SharedCredentialMode::SeparateLogin;
        // `personal` is a codex agent with its own config directory: only a session whose
        // own command is claude launches claude, so only it waits on a claude login.
        c.session
            .custom_agents
            .insert("personal".to_string(), "codex".to_string());
        c.session
            .agent_config_dir
            .insert("personal".to_string(), "~/.claude-personal".to_string());
    })
    .unwrap();
    let ids: Vec<String> = env.view.instances.keys().cloned().collect();
    {
        let own = env.view.instances.get_mut(&ids[0]).unwrap();
        own.tool = "personal".to_string();
        own.command = "claude".to_string();
    }
    {
        let plain = env.view.instances.get_mut(&ids[1]).unwrap();
        plain.tool = "personal".to_string();
        plain.command = String::new();
    }
    let profile = env.view.instances[&ids[0]].effective_profile();

    env.view.selected_session = Some(ids[0].clone());
    assert_eq!(
        env.view.sandbox_login_target(),
        Some(LoginSelection {
            profile: profile.clone(),
            tool: "personal".to_string(),
            command: Some("claude".to_string()),
            recovery: "aoe sandbox login --tool personal --command claude".to_string(),
        }),
        "the session's own command is part of the target"
    );
    env.view.selected_session = Some(ids[1].clone());
    let target = env.view.sandbox_login_target().unwrap();
    assert_eq!(
        (target.tool.as_str(), target.command.as_deref()),
        ("claude", None),
        "a session whose command has no sandbox login falls back to the default tool"
    );
    assert_eq!(target.recovery, "aoe sandbox login --tool claude");

    // The helper reads the file of the session's command, which already holds a login; plain
    // claude's file is not even created.
    let own_file = home.join(".claude-personal/sandbox-v2/.credentials.json");
    std::fs::create_dir_all(own_file.parent().unwrap()).unwrap();
    let expires_at = crate::time::Timestamp::now().as_millis() + 3_600_000;
    std::fs::write(
        &own_file,
        format!(
            r#"{{"claudeAiOauth":{{"accessToken":"fake-access","refreshToken":"fake-refresh","expiresAt":{expires_at}}}}}"#
        ),
    )
    .unwrap();
    env.view.selected_session = Some(ids[0].clone());
    env.view.run_action(ActionId::SandboxLogin, None);
    let event = env
        .view
        .sandbox_login_poller
        .as_ref()
        .unwrap()
        .recv_for_test();
    assert!(
        matches!(
            event,
            LoginEvent::StartFailed {
                failure: StartFailure::AlreadyOk,
                ..
            }
        ),
        "the login was started for the session's command, not the profile's"
    );
    env.view.apply_sandbox_login_event(event);
    let shown: String = screen(&mut env)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        shown.contains("aoe sandbox login --tool personal --command claude"),
        "the dialog says which command line logs in the same file: {shown}"
    );
}

/// A session of a profile other than the default one waits on the file that profile resolves:
/// the action runs the login for the session's profile, and the dialog names `-p <profile>`.
#[test]
#[serial]
fn sandbox_login_runs_for_the_selected_sessions_profile() {
    use crate::tui::home::sandbox_login::LoginSelection;
    use crate::tui::sandbox_login_poller::StartFailure;

    let mut env = create_test_env_with_sessions(1);
    let home = dirs::home_dir().unwrap();
    let (_runtime, _calls) = crate::session::test_support::failing_container_runtime(&home);
    crate::session::config::update_config(|c| {
        c.sandbox.shared_credential = SharedCredentialMode::SeparateLogin;
    })
    .unwrap();
    // Only `work` declares a config directory for claude: its sessions wait on another file.
    let mut work = crate::session::config::profile_config::ProfileConfig::default();
    work.overrides.insert(
        "session".to_string(),
        serde_json::json!({ "agent_config_dir": { "claude": "~/.claude-work" } }),
    );
    crate::session::config::profile_config::save_profile_config("work", &work).unwrap();
    let id = env.view.instances.keys().next().unwrap().clone();
    {
        let session = env.view.instances.get_mut(&id).unwrap();
        session.tool = "claude".to_string();
        session.source_profile = "work".to_string();
    }
    env.view.selected_session = Some(id);
    assert_eq!(
        env.view.sandbox_login_target(),
        Some(LoginSelection {
            profile: "work".to_string(),
            tool: "claude".to_string(),
            command: None,
            recovery: "aoe sandbox login -p work --tool claude".to_string(),
        }),
        "the session's profile is part of the target"
    );

    // Only the file of `work` holds a login: the helper reads it, not the default profile's.
    let work_file = home.join(".claude-work/sandbox-v2/.credentials.json");
    std::fs::create_dir_all(work_file.parent().unwrap()).unwrap();
    crate::session::test_support::write_fake_shared_credential(&work_file);
    env.view.run_action(ActionId::SandboxLogin, None);
    let event = env
        .view
        .sandbox_login_poller
        .as_ref()
        .unwrap()
        .recv_for_test();
    assert!(
        matches!(
            event,
            LoginEvent::StartFailed {
                failure: StartFailure::AlreadyOk,
                ..
            }
        ),
        "the login was started for the session's profile, not the default one"
    );
}

/// A work profile whose sessions wait on a file of their own, with the session selected.
fn select_work_session(env: &mut TestEnv, home: &std::path::Path) -> std::path::PathBuf {
    let mut work = crate::session::config::profile_config::ProfileConfig::default();
    work.overrides.insert(
        "session".to_string(),
        serde_json::json!({ "agent_config_dir": { "claude": "~/.claude-work" } }),
    );
    crate::session::config::profile_config::save_profile_config("work", &work).unwrap();
    let id = env.view.instances.keys().next().unwrap().clone();
    {
        let session = env.view.instances.get_mut(&id).unwrap();
        session.tool = "claude".to_string();
        session.source_profile = "work".to_string();
    }
    env.view.selected_session = Some(id);
    let work_file = home.join(".claude-work/sandbox-v2/.credentials.json");
    std::fs::create_dir_all(work_file.parent().unwrap()).unwrap();
    work_file
}

#[test]
#[serial]
fn sandbox_login_start_again_keeps_the_target_the_dialog_opened_with() {
    use crate::tui::sandbox_login_poller::StartFailure;

    let mut env = create_test_env_with_sessions(1);
    let home = dirs::home_dir().unwrap();
    let (_runtime, _calls) = crate::session::test_support::failing_container_runtime(&home);
    crate::session::config::update_config(|c| {
        c.sandbox.shared_credential = SharedCredentialMode::SeparateLogin;
    })
    .unwrap();
    let work_file = select_work_session(&mut env, &home);
    crate::session::test_support::write_fake_shared_credential(&work_file);
    env.view.run_action(ActionId::SandboxLogin, None);
    let first = env
        .view
        .sandbox_login_poller
        .as_ref()
        .unwrap()
        .recv_for_test();
    assert!(matches!(
        first,
        LoginEvent::StartFailed {
            failure: StartFailure::AlreadyOk,
            ..
        }
    ));

    // A projection refresh moves the selection while the dialog is open.
    env.view.selected_session = None;
    assert_ne!(
        env.view.sandbox_login_target().map(|t| t.profile),
        Some("work".to_string()),
        "the live selection now resolves another target"
    );
    env.view
        .run_sandbox_login_request(crate::tui::dialogs::LoginRequest::StartAgain { force: false });
    let again = env
        .view
        .sandbox_login_poller
        .as_ref()
        .unwrap()
        .recv_for_test();
    assert!(
        matches!(
            again,
            LoginEvent::StartFailed {
                failure: StartFailure::AlreadyOk,
                ..
            }
        ),
        "the retry logs in the file shown at open, not the new selection's"
    );
}

#[test]
#[serial]
fn sandbox_login_action_stays_available_with_a_trashed_row_selected() {
    let home = TempDir::new().unwrap();
    let _home = crate::session::test_support::isolate_home(home.path());
    let mut env = create_test_env_with_sessions(1);
    let id = env.view.instances.keys().next().unwrap().clone();
    env.view.instances.get_mut(&id).unwrap().trashed_at = Some(crate::time::Timestamp::now());
    env.view.selected_session = Some(id);
    assert!(env.view.selected_is_trashed());

    env.view.run_action(ActionId::SandboxLogin, None);
    assert!(
        env.view.sandbox_login_dialog.is_some(),
        "the login changes shared credentials, not the trashed session"
    );
}
