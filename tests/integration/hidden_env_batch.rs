//! `get_hidden_env_batch` against a real tmux server.
//!
//! tmux aborts a `;`-separated command list at the first command that fails,
//! so a session whose key is unset truncates every later segment. The batch
//! must not report those sessions as unset.

use agent_of_empires::tmux::test_support as env;
use serial_test::serial;
use std::process::Command;

struct Cleanup(Vec<String>);

impl Drop for Cleanup {
    fn drop(&mut self) {
        let socket = crate::common::tmux_socket();
        for name in &self.0 {
            let _ = Command::new("tmux")
                .arg("-S")
                .arg(&socket)
                .args(["kill-session", "-t", name])
                .output();
        }
        // tmux removes the socket asynchronously after its last session exits.
        for _ in 0..40 {
            if !socket.exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
    }
}

fn tmux_available() -> bool {
    Command::new("tmux")
        .arg("-V")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[test]
#[serial]
fn batch_reads_sessions_after_one_without_the_key() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let socket = crate::common::tmux_socket();
    let names = ["aoe_hb_a", "aoe_hb_b", "aoe_hb_c"];
    let mut cleanup = Cleanup(Vec::new());
    for name in names {
        let out = Command::new("tmux")
            .arg("-S")
            .arg(&socket)
            .args(["new-session", "-d", "-s", name, "sh"])
            .output()
            .expect("tmux new-session");
        assert!(
            out.status.success(),
            "new-session {name}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        cleanup.0.push(name.to_string());
    }
    // The middle session deliberately has no AOE_INSTANCE_ID, like a paired
    // terminal session.
    env::set_hidden_env(names[0], env::AOE_INSTANCE_ID_KEY, "aaa").unwrap();
    env::set_hidden_env(names[2], env::AOE_INSTANCE_ID_KEY, "ccc").unwrap();

    let got = env::get_hidden_env_batch(&names, env::AOE_INSTANCE_ID_KEY);
    let got: Vec<(&str, Option<&str>)> = got
        .iter()
        .map(|(n, v)| (n.as_str(), v.as_deref()))
        .collect();
    assert_eq!(
        got,
        vec![
            ("aoe_hb_a", Some("aaa")),
            ("aoe_hb_b", None),
            ("aoe_hb_c", Some("ccc")),
        ]
    );
}

/// #3616: `show-environment` emits a multiline value as continuation lines, so
/// an unrelated variable can print a line reading `AOE_INSTANCE_ID=...`. The
/// batch must still report the variable's real value.
#[test]
#[serial]
fn batch_ignores_a_multiline_value_that_imitates_the_key() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let socket = crate::common::tmux_socket();
    let name = "aoe_hb_multiline";
    let out = Command::new("tmux")
        .arg("-S")
        .arg(&socket)
        .args(["new-session", "-d", "-s", name, "sh"])
        .output()
        .expect("tmux new-session");
    assert!(
        out.status.success(),
        "new-session {name}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let _cleanup = Cleanup(vec![name.to_string()]);

    env::set_hidden_env(name, env::AOE_INSTANCE_ID_KEY, "real-id").unwrap();
    env::set_hidden_env(
        name,
        "ZZZ",
        &format!("unrelated\n{}=spoofed-id", env::AOE_INSTANCE_ID_KEY),
    )
    .unwrap();

    let got = env::get_hidden_env_batch(&[name], env::AOE_INSTANCE_ID_KEY);
    assert_eq!(got, vec![(name.to_string(), Some("real-id".to_string()))]);
}
