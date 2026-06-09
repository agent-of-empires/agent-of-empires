//! Shared helpers for integration tests under `tests/`. Each consumer
//! declares `mod common;` to bring this in scope; consumers that do
//! not include this module via `mod common;` simply do not compile
//! these helpers.

use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

/// Bind ephemeral, drop, return the port. Tiny TOCTOU window before the
/// caller binds; acceptable under `#[serial]`. Used by every integration
/// test that spawns an `aoe serve` subprocess.
pub fn pick_free_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    l.local_addr().expect("local_addr").port()
}

/// Poll-connect against `127.0.0.1:port` until success or `deadline`
/// elapses. Returns `true` on success, `false` on timeout. The 100ms
/// inner sleep matches the rest of the test harness; the connect timeout
/// is shorter so the deadline budget is mostly spent retrying rather
/// than blocked on a single slow connect.
pub fn wait_for_port(port: u16, deadline: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < deadline {
        if TcpStream::connect_timeout(
            &format!("127.0.0.1:{}", port).parse().unwrap(),
            Duration::from_millis(200),
        )
        .is_ok()
        {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}
