use super::*;
use crate::session::config::{update_app_state, update_config, AppStateConfig, SidebarPosition};
use crate::tips::SHORTCUT_BAR_TIP_ID;
use crate::tui::home::live_send::{parse_chord, parse_chord_list, LiveSendState, LiveSendTarget};

fn live_state() -> LiveSendState {
    LiveSendState {
        session_id: "unused".into(),
        title: "session".into(),
        tmux_name: "unused".into(),
        target: LiveSendTarget::Agent,
        exit_chords: parse_chord_list("C-q"),
        leader: parse_chord("C-Space"),
    }
}

#[test]
#[serial]
fn hidden_shortcut_bar_gives_its_row_to_content_and_clears_hit_targets() {
    let mut env = create_test_env_with_sessions(2);
    for width in [40, 79, 80, 120] {
        for position in [SidebarPosition::Left, SidebarPosition::Right] {
            for collapsed in [false, true] {
                for live in [false, true] {
                    env.view.sidebar_position = position;
                    env.view.sidebar_collapsed = collapsed;
                    env.view.live_send = live.then(live_state);
                    env.view.show_shortcut_bar = true;
                    let shown = render_home_to_string(&mut env.view, width, 24);
                    let preview_bottom = env.view.preview_outer_area.bottom();
                    let list_bottom = if collapsed {
                        env.view.expand_strip_area.bottom()
                    } else {
                        env.view.list_area.bottom()
                    };
                    let badge = env.view.tips_badge_rect;
                    let buttons = env.view.footer_buttons.clone();
                    assert_eq!(shown.contains("LIVE"), live);
                    env.view.show_shortcut_bar = false;
                    let hidden = render_home_to_string(&mut env.view, width, 24);
                    assert_eq!(env.view.preview_outer_area.bottom(), preview_bottom + 1);
                    if width >= 80 || collapsed {
                        let bottom = if collapsed {
                            env.view.expand_strip_area.bottom()
                        } else {
                            env.view.list_area.bottom()
                        };
                        assert_eq!(bottom, list_bottom + 1);
                    }
                    assert!(!hidden.contains("LIVE"));
                    assert!(!hidden.contains("Ctrl+Q to exit"));
                    assert!(env.view.footer_buttons.is_empty());
                    assert!(env.view.tips_badge_rect.is_none());
                    for (_, rect) in buttons {
                        assert!(env.view.footer_button_at(rect.x, rect.y).is_none());
                    }
                    if let Some(rect) = badge {
                        assert!(!env.view.handle_tips_badge_click(rect.x, rect.y));
                    }
                    env.view.show_shortcut_bar = true;
                    render_home_to_string(&mut env.view, width, 24);
                    assert_eq!(env.view.preview_outer_area.bottom(), preview_bottom);
                }
            }
        }
    }
}

#[test]
#[serial]
fn hidden_bar_preserves_temporary_feedback_and_keyboard_actions() {
    let mut env = create_test_env_with_sessions(2);
    env.view.show_shortcut_bar = false;
    env.view.flash_status("Copied selection");
    let screen = render_home_to_string(&mut env.view, 120, 24);
    assert!(screen.contains("Copied selection"));
    assert_eq!(env.view.preview_outer_area.bottom(), 23);
    env.view.status_flash.as_mut().unwrap().1 = std::time::Instant::now();
    assert!(env.view.expire_status_flash());
    render_home_to_string(&mut env.view, 120, 24);
    assert_eq!(env.view.preview_outer_area.bottom(), 24);

    env.view.handle_key(key(KeyCode::Char('?')), None);
    assert!(env.view.show_help);
    env.view.handle_key(key(KeyCode::Esc), None);
    env.view.handle_key(
        KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL),
        None,
    );
    assert!(env.view.command_palette.is_some());
    env.view.handle_key(key(KeyCode::Esc), None);
    env.view.open_tips_dialog();
    assert!(env.view.tips_dialog.is_some());
    env.view.handle_key(key(KeyCode::Esc), None);

    env.view.live_send = Some(live_state());
    env.view.live_send_pending_leader = true;
    let screen = render_home_to_string(&mut env.view, 120, 24);
    assert!(screen.contains("k palette"));
    env.view.live_send_pending_leader = false;
    env.view.flash_ctrl_c_hint();
    let screen = render_home_to_string(&mut env.view, 120, 24);
    assert!(screen.contains("Ctrl+C sent to agent"));
    env.view.live_send_ctrl_c_flash_until = None;
    env.view.handle_key(
        KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL),
        None,
    );
    assert!(env.view.live_send.is_none());
}

#[test]
#[serial]
fn shortcut_bar_setting_reloads_and_is_global_across_profiles() {
    let mut env = create_test_env_empty();
    assert!(env.view.show_shortcut_bar);
    let profile_dir = crate::session::get_app_dir().unwrap().join("profiles/test");
    std::fs::write(
        profile_dir.join("config.toml"),
        "[session]\nshow_shortcut_bar = true\n",
    )
    .unwrap();
    update_config(|config| config.session.show_shortcut_bar = false).unwrap();
    env.view.try_refresh_from_config_watcher().unwrap();
    assert!(!env.view.show_shortcut_bar);
    assert!(!test_view(Some("test")).show_shortcut_bar);
    seed_profile("other", &[]);
    assert!(!test_view(Some("other")).show_shortcut_bar);
    update_config(|config| config.session.show_shortcut_bar = true).unwrap();
    env.view.try_refresh_from_config_watcher().unwrap();
    assert!(env.view.show_shortcut_bar);
}

#[test]
#[serial]
fn shortcut_bar_tip_waits_for_idle_and_persists_seen_state() {
    let mut env = create_test_env_empty();
    update_app_state(|state| state.sessions_created = 30).unwrap();
    env.view.refresh_shortcut_bar_tip();
    assert!(!env.view.try_present_shortcut_bar_tip());
    crate::tips::record_session_creations(1);
    env.view.reload().unwrap();
    assert_eq!(
        env.view.pending_tip_pop.map(|tip| tip.id),
        Some(SHORTCUT_BAR_TIP_ID)
    );
    env.view.show_help = true;
    assert!(!env.view.try_present_shortcut_bar_tip());
    env.view.show_help = false;
    env.view.search_active = true;
    assert!(!env.view.try_present_shortcut_bar_tip());
    env.view.search_active = false;
    env.view.live_send = Some(live_state());
    assert!(!env.view.try_present_shortcut_bar_tip());
    env.view.live_send = None;
    let mut structured = crate::tui::structured_view::embedded::EmbeddedView::for_test("test");
    structured.activate();
    env.view.structured_preview = Some(structured);
    assert!(!env.view.try_present_shortcut_bar_tip());
    env.view.structured_preview.as_mut().unwrap().deactivate();

    // A queued tip must not swallow the key that opens another action.
    env.view.handle_key(key(KeyCode::Char('?')), None);
    assert!(env.view.show_help);
    env.view.handle_key(key(KeyCode::Esc), None);
    assert!(env.view.try_present_shortcut_bar_tip());
    let screen = render_home_to_string(&mut env.view, 120, 28);
    assert!(screen.contains("More room for your sessions"));
    assert!(env.view.show_shortcut_bar);
    env.view.handle_key(key(KeyCode::Esc), None);
    assert!(AppStateConfig::load()
        .unwrap()
        .tips_seen
        .iter()
        .any(|id| id == SHORTCUT_BAR_TIP_ID));
    env.view.reload().unwrap();
    assert!(!env.view.try_present_shortcut_bar_tip());
    let mut restarted = test_view(Some("test"));
    assert!(!restarted.try_present_shortcut_bar_tip());
}

#[test]
#[serial]
fn queued_shortcut_tip_rechecks_disabled_hidden_and_seen_state() {
    let mut env = create_test_env_empty();
    for suppression in ["disabled", "hidden", "seen"] {
        update_app_state(|state| {
            state.sessions_created = 31;
            state.tips_seen.clear();
        })
        .unwrap();
        update_config(|config| {
            config.session.show_tips = true;
            config.session.show_shortcut_bar = true;
        })
        .unwrap();
        env.view.refresh_shortcut_bar_tip();
        assert!(env.view.pending_tip_pop.is_some());
        match suppression {
            "disabled" => update_config(|config| config.session.show_tips = false).unwrap(),
            "hidden" => update_config(|config| config.session.show_shortcut_bar = false).unwrap(),
            _ => {
                update_app_state(|state| state.tips_seen.push(SHORTCUT_BAR_TIP_ID.into())).unwrap()
            }
        }
        assert!(!env.view.try_present_shortcut_bar_tip(), "{suppression}");
        env.view.refresh_shortcut_bar_tip();
        assert!(env.view.pending_tip_pop.is_none(), "{suppression}");
    }
}
