//! Test fixtures shared by more than one submodule's tests.

pub(super) fn vecs(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

/// `MakeWriter` sink so a test can read back exactly the bytes a daemon would
/// have appended to `debug.log`.
#[derive(Clone)]
struct LogSink(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for LogSink {
    fn write(&mut self, src: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(src);
        Ok(src.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Captures every event on this thread, formatted as in `debug.log`, until the
/// guard drops.
pub(super) fn capture_logs() -> (
    tracing::subscriber::DefaultGuard,
    std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
) {
    let buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
    let sink = LogSink(buf.clone());
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(move || sink.clone())
        .with_ansi(false)
        .event_format(crate::logging::NoSpanFormat)
        .finish();
    (tracing::subscriber::set_default(subscriber), buf)
}
