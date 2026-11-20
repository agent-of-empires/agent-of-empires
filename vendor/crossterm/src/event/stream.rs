use std::{
    io,
    pin::Pin,
    sync::{
        mpsc::{self, Receiver, SyncSender},
        Arc, Mutex,
    },
    task::{Context, Poll},
    thread,
    time::Duration,
};

use futures_core::stream::Stream;

use crate::event::{
    filter::EventFilter, lock_internal_event_reader, poll_internal, read_internal, sys::Waker,
    Event, InternalEvent,
};

/// A stream of `Result<Event>`.
///
/// **This type is not available by default. You have to use the `event-stream` feature flag
/// to make it available.**
///
/// It implements the [Stream](futures_core::stream::Stream)
/// trait and allows you to receive [`Event`]s with [`async-std`](https://crates.io/crates/async-std)
/// or [`tokio`](https://crates.io/crates/tokio) crates.
///
/// Check the [examples](https://github.com/crossterm-rs/crossterm/tree/master/examples) folder to see how to use
/// it (`event-stream-*`).
///
/// Input errors are yielded once, then the stream ends. Construct a new stream
/// after restoring terminal access.
#[derive(Debug)]
pub struct EventStream {
    poll_internal_waker: Waker,
    shared: Arc<Mutex<StreamState>>,
    task_sender: SyncSender<()>,
}

impl Default for EventStream {
    fn default() -> Self {
        let (task_sender, receiver) = mpsc::sync_channel(1);
        let shared = Arc::new(Mutex::new(StreamState::default()));
        let worker_state = Arc::clone(&shared);
        thread::spawn(move || {
            run_worker(receiver, worker_state, || poll_internal(None, &EventFilter));
        });

        EventStream {
            poll_internal_waker: lock_internal_event_reader().waker(),
            shared,
            task_sender,
        }
    }
}

impl EventStream {
    /// Constructs a new instance of `EventStream`.
    pub fn new() -> EventStream {
        EventStream::default()
    }
}

#[derive(Debug, Default)]
struct StreamState {
    running: bool,
    shutdown: bool,
    terminal: bool,
    error: Option<io::Error>,
    waker: Option<std::task::Waker>,
}

fn run_worker(
    receiver: Receiver<()>,
    shared: Arc<Mutex<StreamState>>,
    mut poll_input: impl FnMut() -> io::Result<bool>,
) {
    while receiver.recv().is_ok() {
        if shared.lock().unwrap().shutdown {
            return;
        }
        let error = loop {
            match poll_input() {
                Ok(true) => break None,
                Err(error) => break Some(error),
                Ok(false) => {
                    if shared.lock().unwrap().shutdown {
                        return;
                    }
                }
            }
        };
        let (waker, finished) = {
            let mut state = shared.lock().unwrap();
            state.terminal = error.is_some();
            state.error = error;
            state.running = false;
            (state.waker.take(), state.terminal || state.shutdown)
        };
        if let Some(waker) = waker {
            waker.wake();
        }
        if finished {
            return;
        }
    }
}

fn poll_next_ready(
    shared: &Mutex<StreamState>,
    sender: &SyncSender<()>,
    cx: &mut Context<'_>,
    read_ready: impl FnOnce() -> io::Result<Option<Event>>,
) -> Poll<Option<io::Result<Event>>> {
    {
        let mut state = shared.lock().unwrap();
        if let Some(error) = state.error.take() {
            return Poll::Ready(Some(Err(error)));
        }
        if state.terminal {
            return Poll::Ready(None);
        }
        if !matches!(state.waker.as_ref(), Some(waker) if waker.will_wake(cx.waker())) {
            state.waker = Some(cx.waker().clone());
        }
        if state.running {
            return Poll::Pending;
        }
    }
    match read_ready() {
        Ok(Some(event)) => Poll::Ready(Some(Ok(event))),
        Err(error) => {
            shared.lock().unwrap().terminal = true;
            Poll::Ready(Some(Err(error)))
        }
        Ok(None) => {
            let mut state = shared.lock().unwrap();
            state.running = true;
            if sender.send(()).is_err() {
                state.running = false;
                state.terminal = true;
                return Poll::Ready(Some(Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "Terminal input worker stopped",
                ))));
            }
            Poll::Pending
        }
    }
}

impl Stream for EventStream {
    type Item = io::Result<Event>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        poll_next_ready(&self.shared, &self.task_sender, cx, || {
            if !poll_internal(Some(Duration::ZERO), &EventFilter)? {
                return Ok(None);
            }
            match read_internal(&EventFilter)? {
                InternalEvent::Event(event) => Ok(Some(event)),
                #[cfg(unix)]
                _ => unreachable!(),
            }
        })
    }
}

impl Drop for EventStream {
    fn drop(&mut self) {
        self.shared.lock().unwrap().shutdown = true;
        let _ = self.poll_internal_waker.wake();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::Wake;

    #[derive(Default)]
    struct WakeCount(AtomicUsize);

    impl Wake for WakeCount {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn worker_error_reaches_latest_task_once_then_stream_ends() {
        for error in [
            io::Error::new(io::ErrorKind::UnexpectedEof, "closed input"),
            io::Error::from_raw_os_error(5),
        ] {
            let expected = (error.kind(), error.raw_os_error());
            let shared = Arc::new(Mutex::new(StreamState::default()));
            let (sender, receiver) = mpsc::sync_channel(1);
            let (entered, started) = mpsc::channel();
            let (release, gate) = mpsc::channel();
            let worker_state = Arc::clone(&shared);
            let mut error = Some(error);
            let worker = thread::spawn(move || {
                run_worker(receiver, worker_state, || {
                    entered.send(()).unwrap();
                    gate.recv_timeout(Duration::from_secs(2)).unwrap();
                    Err(error.take().expect("input error must not be retried"))
                });
            });
            let first = Arc::new(WakeCount::default());
            let latest = Arc::new(WakeCount::default());
            let first_waker = std::task::Waker::from(Arc::clone(&first));
            let latest_waker = std::task::Waker::from(Arc::clone(&latest));
            let mut first_context = Context::from_waker(&first_waker);
            let mut latest_context = Context::from_waker(&latest_waker);
            assert!(
                poll_next_ready(&shared, &sender, &mut first_context, || Ok(None)).is_pending()
            );
            started.recv_timeout(Duration::from_secs(1)).unwrap();
            assert!(poll_next_ready(&shared, &sender, &mut latest_context, || {
                panic!("active worker owns terminal input")
            })
            .is_pending());
            release.send(()).unwrap();
            worker.join().unwrap();
            assert_eq!(first.0.load(Ordering::SeqCst), 0);
            assert_eq!(latest.0.load(Ordering::SeqCst), 1);
            match poll_next_ready(&shared, &sender, &mut latest_context, || {
                panic!("stored error must not repoll terminal input")
            }) {
                Poll::Ready(Some(Err(error))) => {
                    assert_eq!((error.kind(), error.raw_os_error()), expected);
                }
                result => panic!("expected terminal error: {result:?}"),
            }
            assert!(matches!(
                poll_next_ready(&shared, &sender, &mut latest_context, || {
                    panic!("ended stream must not repoll terminal input")
                }),
                Poll::Ready(None)
            ));
        }
    }

    #[test]
    fn shutdown_before_worker_start_does_not_read_input() {
        let shared = Arc::new(Mutex::new(StreamState {
            shutdown: true,
            ..StreamState::default()
        }));
        let (sender, receiver) = mpsc::sync_channel(1);
        sender.send(()).unwrap();
        run_worker(receiver, shared, || {
            panic!("dropped stream must not start another input read")
        });
    }
}
