//! `aoe acp lint-stream` exits 0 on a conformant stream and 1 on findings.

use std::io::Write;
use std::process::{Command, Stdio};

const VALID: &str = r####"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentInfo":{"name":"claude-agent-acp","version":"0.99.1"},"_meta":{"aoe":{"proxy":{"plugin_id":"example.plugin","protocol_version":1,"primitive_schema_version":1}}}}}
{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"tool_call","toolCallId":"t1","title":"Task","_meta":{"aoe":{"v":1,"key":"call-t1","parent_tool_call":"t0"}}}}}
{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"hi"},"_meta":{"aoe":{"v":1,"key":"busy-1","lease":{"id":"bg-1","kind":"busy","ttl_ms":30000}}}}}}
{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"later"},"_meta":{"aoe":{"v":1,"key":"tail-1","after_adapter_sequence":2,"ephemeral":true}}}}}
"####;

const NO_HELLO: &str = r####"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"tool_call","toolCallId":"t1","title":"Task","_meta":{"aoe":{"v":1,"key":"call-t1"}}}}}
"####;

const INVALID: &str = r####"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"_meta":{"aoe":{"proxy":{"plugin_id":"example.plugin","protocol_version":9,"primitive_schema_version":1}}}}}
{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"tool_call","toolCallId":"t1","title":"Task","_meta":{"aoe":{"v":1}}}}}
"####;

fn lint(stream: &str, flags: &[&str]) -> (Option<i32>, String) {
    let home = tempfile::tempdir().expect("temp home");
    let mut child = Command::new(env!("CARGO_BIN_EXE_aoe"))
        .args(["acp", "lint-stream"])
        .args(flags)
        .arg("-")
        .env("HOME", home.path())
        .env("XDG_CONFIG_HOME", home.path().join("config"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("run aoe");
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(stream.as_bytes())
        .expect("write stream");
    let out = child.wait_with_output().expect("wait for aoe");
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

#[test]
fn exit_code_follows_the_findings() {
    let (code, stdout) = lint(VALID, &["--require-hello"]);
    assert_eq!((code, stdout.as_str()), (Some(0), ""));

    let (code, stdout) = lint(NO_HELLO, &[]);
    assert_eq!((code, stdout.as_str()), (Some(0), ""));
    let (code, stdout) = lint(NO_HELLO, &["--require-hello"]);
    assert_eq!(code, Some(1));
    assert!(stdout.contains("hello_missing"), "{stdout}");

    let (code, stdout) = lint(INVALID, &["--require-hello"]);
    assert_eq!(code, Some(1));
    assert!(stdout.contains("invalid_hello"), "{stdout}");
    assert!(stdout.contains("bad_envelope"), "{stdout}");
}
