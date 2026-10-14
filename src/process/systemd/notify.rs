//! Bounded daemon notifications over Unix datagram sockets.

pub(crate) async fn send(message: impl AsRef<[u8]> + Send + 'static) -> std::io::Result<()> {
    let Some(destination) = std::env::var_os("NOTIFY_SOCKET").filter(|path| !path.is_empty())
    else {
        return Ok(());
    };
    tokio::task::spawn_blocking(move || {
        let socket = std::os::unix::net::UnixDatagram::unbound()?;
        #[cfg(target_os = "linux")]
        crate::process::platform::connect_systemd_socket(&socket, &destination)?;
        #[cfg(not(target_os = "linux"))]
        socket.connect(&destination)?;
        socket.set_write_timeout(Some(std::time::Duration::from_secs(1)))?;
        socket.send(message.as_ref()).map(|_| ())
    })
    .await
    .map_err(std::io::Error::other)?
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::os::unix::net::UnixDatagram;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    #[test]
    #[serial_test::parallel]
    fn send_waits_for_capacity_with_a_bounded_timeout() {
        let _exclusion = crate::session::test_support::EnvGuard::read_lock();
        for recover in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("notify.sock");
            let receiver = UnixDatagram::bind(&path).unwrap();
            receiver
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                // Fresh senders fill the receiver, not their own send buffers.
                let sender = UnixDatagram::unbound().unwrap();
                sender.set_nonblocking(true).unwrap();
                sender.connect(&path).unwrap();
                match sender.send(b"occupied") {
                    Ok(_) => assert!(Instant::now() < deadline, "receiver queue did not fill"),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(error) => panic!("fill receiver queue: {error}"),
                }
            }
            let _env =
                crate::session::test_support::EnvGuard::set(&[("NOTIFY_SOCKET", path.as_os_str())]);
            let (started, entered) = mpsc::sync_channel(0);
            let (sent, received) = mpsc::channel();
            let worker = std::thread::spawn(move || {
                let result = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(async {
                        let (result, ()) = tokio::join!(
                            biased;
                            send(b"READY=1\n"),
                            async { let _ = started.send(()); }
                        );
                        result
                    });
                let _ = sent.send(result);
            });
            let entry = entered.recv_timeout(Duration::from_secs(5));
            drop(entered);
            let early = if entry.is_ok() {
                received.recv_timeout(Duration::from_millis(100))
            } else {
                Err(mpsc::RecvTimeoutError::Disconnected)
            };
            let pending = matches!(&early, Err(mpsc::RecvTimeoutError::Timeout));
            if recover {
                let mut occupied = [0u8; 32];
                let n = receiver.recv(&mut occupied).unwrap();
                assert_eq!(&occupied[..n], b"occupied");
            }
            let result = if pending {
                received.recv_timeout(Duration::from_secs(5))
            } else {
                early
            };
            if result.is_err() && !recover {
                // Release an unbounded write before joining its worker.
                receiver.recv(&mut [0u8; 32]).unwrap();
            }
            let joined = worker.join();
            assert!(entry.is_ok(), "notifier did not enter: {entry:?}");
            joined.unwrap();
            assert!(
                pending,
                "recover={recover}: notifier returned while the queue stayed full: {result:?}"
            );
            if recover {
                assert!(
                    matches!(&result, Ok(Ok(()))),
                    "notification failed: {result:?}"
                );
                receiver.set_nonblocking(true).unwrap();
                let mut ready = 0;
                let mut message = [0u8; 32];
                loop {
                    match receiver.recv(&mut message) {
                        Ok(n) if &message[..n] == b"occupied" => {}
                        Ok(n) => {
                            assert_eq!(&message[..n], b"READY=1\n");
                            ready += 1;
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                        Err(error) => panic!("receive notification: {error}"),
                    }
                }
                assert_eq!(ready, 1);
            } else {
                assert!(
                    matches!(&result, Ok(Err(error)) if error.kind() == std::io::ErrorKind::WouldBlock),
                    "a persistently full receiver must time out: {result:?}"
                );
            }
        }
    }
}
