//! JSON-RPC batch support for direct agent stdio. SDK 5 rejects every array,
//! so incoming batches are split into single lines and the replies to their
//! requests are regrouped into one array line.

use agent_client_protocol::{RawJsonRpcMessage, TransportFrame};
use serde_json::Value;
use std::collections::{HashMap, HashSet, VecDeque};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use tokio::io::{
    AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, DuplexStream, ReadHalf,
    WriteHalf,
};
use tokio::sync::mpsc;
use tokio_util::sync::{CancellationToken, DropGuard};

#[derive(Default)]
struct Batches {
    next: u64,
    /// Request id (as JSON text) to the batch awaiting its reply.
    owner: HashMap<String, u64>,
    /// Outstanding request ids sent outside a batch.
    standalone: HashSet<String>,
    open: HashMap<u64, Open>,
    /// Who is owed each `id: null` reply, in the order the SDK answers them:
    /// a batch, or `None` for a standalone line.
    anonymous_queue: VecDeque<Option<u64>>,
}

struct Open {
    named: usize,
    anonymous: usize,
    replies: Vec<Value>,
}

type Shared = Arc<Mutex<Batches>>;

/// Refusals waiting for the write pump. Overflow means the agent is flooding
/// duplicate ids while not reading stdin; the transport is closed instead.
const REFUSAL_CAPACITY: usize = 256;

/// The end the SDK reads and writes. Dropping it retires both pumps, whatever
/// they are blocked on.
pub(super) struct Sdk {
    io: DuplexStream,
    _retire: DropGuard,
}

impl AsyncRead for Sdk {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.io).poll_read(cx, buf)
    }
}

impl AsyncWrite for Sdk {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.io).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.io).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.io).poll_shutdown(cx)
    }
}

/// Sits between the agent's stdio and the SDK.
pub(super) fn wrap<R, W>(agent_out: R, agent_in: W) -> Sdk
where
    R: AsyncRead + Send + Unpin + 'static,
    W: AsyncWrite + Send + Unpin + 'static,
{
    let (ours, sdk) = tokio::io::duplex(64 * 1024);
    let (from_sdk, to_sdk) = tokio::io::split(ours);
    let state = Shared::default();
    // Replies the read pump answers itself, for the write pump to send.
    let (ready_tx, ready_rx) = mpsc::channel(REFUSAL_CAPACITY);
    // Either pump exiting, or the SDK end dropping, retires the other.
    let broken = CancellationToken::new();
    tokio::spawn(split_incoming(
        agent_out,
        to_sdk,
        state.clone(),
        ready_tx,
        broken.clone(),
    ));
    let retire = broken.clone().drop_guard();
    tokio::spawn(group_outgoing(from_sdk, agent_in, state, ready_rx, broken));
    Sdk {
        io: sdk,
        _retire: retire,
    }
}

async fn split_incoming<R: AsyncRead + Unpin>(
    agent_out: R,
    mut to_sdk: WriteHalf<DuplexStream>,
    state: Shared,
    ready: mpsc::Sender<Value>,
    broken: CancellationToken,
) {
    let mut lines = BufReader::new(agent_out).lines();
    'pump: loop {
        let line = tokio::select! {
            _ = broken.cancelled() => break,
            line = lines.next_line() => match line {
                Ok(Some(line)) => line,
                _ => break,
            },
        };
        let singles = match serde_json::from_str::<Value>(line.trim()) {
            Ok(Value::Array(items)) if !items.is_empty() => {
                let (forward, answer) = admit_batch(&state, items);
                if let Some(answer) = answer {
                    if !queue_refusal(&ready, answer) {
                        break 'pump;
                    }
                }
                forward
            }
            // An empty array is not a batch; the SDK answers it with one error.
            _ => match admit_standalone(&state, &line) {
                Ok(()) => vec![line],
                Err(rejection) => {
                    if !queue_refusal(&ready, rejection) {
                        break 'pump;
                    }
                    continue;
                }
            },
        };
        for single in singles {
            if to_sdk
                .write_all(format!("{single}\n").as_bytes())
                .await
                .is_err()
            {
                break 'pump;
            }
        }
    }
    // The SDK must see EOF, not a quiet pipe.
    let _ = to_sdk.shutdown().await;
}

/// Hands a reply to the write pump without waiting: a blocked send could
/// deadlock against an agent that stalls stdin while stdout is full.
fn queue_refusal(ready: &mpsc::Sender<Value>, reply: Value) -> bool {
    let queued = ready.try_send(reply).is_ok();
    if !queued {
        tracing::warn!(target: "acp.protocol", "cannot queue a refusal, closing the agent transport");
    }
    queued
}

/// What the SDK answers for one entry. Entries it cannot attribute to an id
/// (malformed, non-objects, null-id requests) get an `id: null` error.
enum Expect {
    Nothing,
    Named(String),
    Anonymous,
}

fn expected_reply(entry: &Value) -> Expect {
    let response_only = entry.as_object().is_some_and(|object| {
        !object.contains_key("method")
            && (object.contains_key("result") || object.contains_key("error"))
    });
    match TransportFrame::parse_json(&entry.to_string()) {
        TransportFrame::Single(RawJsonRpcMessage::Request(request)) => {
            match serde_json::to_value(&request.id) {
                Ok(id) if !id.is_null() => Expect::Named(id.to_string()),
                _ => Expect::Anonymous,
            }
        }
        TransportFrame::Malformed { .. } if !response_only => Expect::Anonymous,
        _ => Expect::Nothing,
    }
}

/// Replies cannot be matched to two outstanding requests with one id, so the
/// later one is refused instead of reaching the SDK.
fn duplicate_id_error(id: &str) -> Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": serde_json::from_str::<Value>(id).unwrap_or(Value::Null),
        "error": {
            "code": -32600,
            "message": "Invalid request",
            "data": "duplicate request id",
        },
    })
}

/// Registers a batch. Returns the entries to forward to the SDK, and the
/// finished reply when nothing forwarded is left to wait for.
fn admit_batch(state: &Shared, items: Vec<Value>) -> (Vec<String>, Option<Value>) {
    let mut batches = state.lock().expect("batch state poisoned");
    let mut forward = Vec::new();
    let mut named = Vec::new();
    let mut seen = HashSet::new();
    let mut anonymous = 0;
    let mut refused = Vec::new();
    for item in items {
        match expected_reply(&item) {
            Expect::Named(id) => {
                let taken = batches.owner.contains_key(&id)
                    || batches.standalone.contains(&id)
                    || !seen.insert(id.clone());
                if taken {
                    refused.push(duplicate_id_error(&id));
                    continue;
                }
                named.push(id);
            }
            Expect::Anonymous => anonymous += 1,
            Expect::Nothing => {}
        }
        forward.push(item.to_string());
    }
    if named.is_empty() && anonymous == 0 {
        let answer = (!refused.is_empty()).then(|| Value::Array(refused));
        return (forward, answer);
    }
    let batch = batches.next;
    batches.next += 1;
    batches.open.insert(
        batch,
        Open {
            named: named.len(),
            anonymous,
            replies: refused,
        },
    );
    for id in named {
        batches.owner.insert(id, batch);
    }
    batches
        .anonymous_queue
        .extend(std::iter::repeat_n(Some(batch), anonymous));
    (forward, None)
}

/// Tracks a line outside any batch: it can owe an `id: null` reply, and its
/// request id must not collide with one a batch is waiting on. `Err` carries
/// the refusal to send instead of forwarding the line.
fn admit_standalone(state: &Shared, line: &str) -> Result<(), Value> {
    let mut batches = state.lock().expect("batch state poisoned");
    let expect = match serde_json::from_str::<Value>(line.trim()) {
        Ok(Value::Array(_)) | Err(_) => Expect::Anonymous,
        Ok(value) => expected_reply(&value),
    };
    match expect {
        Expect::Named(id) => {
            if batches.owner.contains_key(&id) || !batches.standalone.insert(id.clone()) {
                return Err(duplicate_id_error(&id));
            }
        }
        Expect::Anonymous => batches.anonymous_queue.push_back(None),
        Expect::Nothing => {}
    }
    Ok(())
}

async fn group_outgoing<W: AsyncWrite + Unpin>(
    from_sdk: ReadHalf<DuplexStream>,
    mut agent_in: W,
    state: Shared,
    mut ready: mpsc::Receiver<Value>,
    broken: CancellationToken,
) {
    let _retire = broken.clone().drop_guard();
    let mut lines = BufReader::new(from_sdk).lines();
    loop {
        let out = tokio::select! {
            line = lines.next_line() => match line {
                Ok(Some(line)) => match serde_json::from_str::<Value>(line.trim()) {
                    Ok(reply) => match claim_reply(&state, reply) {
                        Claimed::Held => continue,
                        Claimed::Complete(replies) => Value::Array(replies).to_string(),
                        Claimed::Single => line,
                    },
                    Err(_) => line,
                },
                _ => return,
            },
            Some(answer) = ready.recv() => answer.to_string(),
        };
        let line = format!("{out}\n");
        let written = tokio::select! {
            _ = broken.cancelled() => return,
            written = agent_in.write_all(line.as_bytes()) => written,
        };
        if let Err(error) = written {
            tracing::debug!(target: "acp.protocol", %error, "agent stdin write failed");
            return;
        }
    }
}

enum Claimed {
    /// Not part of a batch.
    Single,
    /// Part of a batch that still waits for other replies.
    Held,
    /// The last reply of a batch.
    Complete(Vec<Value>),
}

fn claim_reply(state: &Shared, reply: Value) -> Claimed {
    if reply.get("method").is_some() {
        return Claimed::Single;
    }
    let mut batches = state.lock().expect("batch state poisoned");
    let batch = match reply.get("id") {
        None => return Claimed::Single,
        Some(id) if id.is_null() => {
            let Some(Some(batch)) = batches.anonymous_queue.pop_front() else {
                return Claimed::Single;
            };
            batches
                .open
                .get_mut(&batch)
                .expect("queued batch is open")
                .anonymous -= 1;
            batch
        }
        Some(id) => {
            let id = id.to_string();
            let Some(batch) = batches.owner.remove(&id) else {
                batches.standalone.remove(&id);
                return Claimed::Single;
            };
            batches
                .open
                .get_mut(&batch)
                .expect("owned batch is open")
                .named -= 1;
            batch
        }
    };
    let open = batches.open.get_mut(&batch).expect("claimed batch is open");
    open.replies.push(reply);
    if open.named + open.anonymous > 0 {
        return Claimed::Held;
    }
    let done = batches.open.remove(&batch).expect("claimed batch is open");
    Claimed::Complete(done.replies)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::ProtocolVersion;
    use agent_client_protocol::v1::schema::InitializeRequest;
    use agent_client_protocol::{ByteStreams, Client};
    use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

    async fn read_value(lines: &mut tokio::io::Lines<impl AsyncBufReadExt + Unpin>) -> Value {
        let line = tokio::time::timeout(std::time::Duration::from_secs(5), lines.next_line())
            .await
            .expect("a line arrives")
            .unwrap()
            .expect("stream open");
        serde_json::from_str(&line).unwrap()
    }

    #[tokio::test]
    async fn batches_are_split_and_their_replies_regrouped() {
        let (agent_side_out, agent_out) = tokio::io::duplex(4096);
        let (agent_in, agent_side_in) = tokio::io::duplex(4096);
        let sdk = wrap(agent_out, agent_in);
        let (sdk_read, mut sdk_write) = tokio::io::split(sdk);
        let mut incoming = BufReader::new(sdk_read).lines();
        let mut outgoing = BufReader::new(agent_side_in).lines();
        let mut agent_side_out = agent_side_out;

        let batch = r#"[{"jsonrpc":"2.0","id":1,"method":"a"},{"jsonrpc":"2.0","method":"n"},{"jsonrpc":"2.0","id":"x","method":"b"}]"#;
        let single = r#"{"jsonrpc":"2.0","id":9,"method":"c"}"#;
        agent_side_out
            .write_all(format!("{batch}\n{single}\n").as_bytes())
            .await
            .unwrap();
        let mut methods = Vec::new();
        for _ in 0..4 {
            methods.push(read_value(&mut incoming).await["method"].clone());
        }
        assert_eq!(methods, ["a", "n", "b", "c"]);

        // Replies out of order; the single request is answered on its own.
        for reply in [
            r#"{"jsonrpc":"2.0","id":9,"result":0}"#,
            r#"{"jsonrpc":"2.0","id":"x","result":2}"#,
            r#"{"jsonrpc":"2.0","id":1,"result":1}"#,
        ] {
            sdk_write
                .write_all(format!("{reply}\n").as_bytes())
                .await
                .unwrap();
        }
        assert_eq!(read_value(&mut outgoing).await["id"], 9);
        let grouped = read_value(&mut outgoing).await;
        let ids: Vec<_> = grouped
            .as_array()
            .expect("one array reply")
            .iter()
            .map(|r| r["id"].clone())
            .collect();
        assert_eq!(ids, [serde_json::json!("x"), serde_json::json!(1)]);
    }

    #[tokio::test]
    async fn malformed_and_null_id_entries_are_answered_inside_the_batch_reply() {
        let (mut agent_side_out, agent_out) = tokio::io::duplex(4096);
        let (agent_in, agent_side_in) = tokio::io::duplex(4096);
        let (read, write) = tokio::io::split(wrap(agent_out, agent_in));
        let batch = serde_json::json!([
            {"jsonrpc": "2.0", "id": 1, "method": "x/unknown"},
            {"jsonrpc": "2.0", "id": 2, "method": "x/unknown", "result": 1},
            {"jsonrpc": "2.0", "id": null, "method": "x/unknown"},
            5,
            {"jsonrpc": "2.0", "method": "x/notification"},
            {"jsonrpc": "2.0", "id": 3, "result": 1},
        ]);
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        let agent = async move {
            agent_side_out
                .write_all(format!("{batch}\n").as_bytes())
                .await
                .unwrap();
            let mut replies = BufReader::new(agent_side_in).lines();
            let reply = read_value(&mut replies).await;
            let entries = reply.as_array().expect("one array reply");
            // Request 1, the malformed entry, the null-id request and `5`.
            assert_eq!(entries.len(), 4, "{reply}");
            assert_eq!(entries[0..1].iter().filter(|e| e["id"] == 1).count(), 1);
            assert_eq!(entries.iter().filter(|e| e["id"].is_null()).count(), 3);
            let _ = done_tx.send(());
        };
        let client = Client.v1().connect_with(
            ByteStreams::new(write.compat_write(), read.compat()),
            async move |_cx| {
                // Returning earlier would shut the connection down under the peer.
                let _ = done_rx.await;
                Ok(())
            },
        );
        let (result, ()) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            tokio::join!(client, agent)
        })
        .await
        .expect("the batch is answered");
        result.unwrap();
    }

    #[tokio::test]
    async fn a_failed_agent_write_closes_the_sdk_transport() {
        let (_agent_side_out, agent_out) = tokio::io::duplex(4096);
        let (agent_in, agent_side_in) = tokio::io::duplex(4096);
        drop(agent_side_in);
        let mut sdk = wrap(agent_out, agent_in);
        sdk.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"x\"}\n")
            .await
            .unwrap();
        let mut buf = Vec::new();
        let read = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            tokio::io::AsyncReadExt::read_to_end(&mut sdk, &mut buf),
        )
        .await
        .expect("the SDK end closes while the agent's stdout stays open");
        assert_eq!(read.unwrap(), 0);
    }

    #[tokio::test]
    async fn agent_stdout_is_released_after_the_sdk_end_closes() {
        let (mut agent_side_out, agent_out) = tokio::io::duplex(4096);
        let (agent_in, _agent_side_in) = tokio::io::duplex(4096);
        drop(wrap(agent_out, agent_in));
        // The pump must stop reading, dropping its end, so writes start to fail.
        let released = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while agent_side_out.write_all(b"{}\n").await.is_ok() {
                tokio::task::yield_now().await;
            }
        })
        .await;
        assert!(released.is_ok(), "the agent's stdout was never released");
    }

    #[tokio::test]
    async fn an_idle_agent_stdout_is_released_when_the_sdk_end_drops() {
        let (mut agent_side_out, agent_out) = tokio::io::duplex(4096);
        let (agent_in, _agent_side_in) = tokio::io::duplex(4096);
        drop(wrap(agent_out, agent_in));
        // No line is sent, so the pump has nothing to fail on: it must notice alone.
        let mut seen = Vec::new();
        let read = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            tokio::io::AsyncReadExt::read_to_end(&mut agent_side_out, &mut seen),
        )
        .await
        .expect("the idle pump kept the agent's stdout");
        assert_eq!(read.unwrap(), 0);
    }

    /// Signals its first write, then never completes one; the channel closes
    /// when the writer is dropped.
    struct Stalled(mpsc::Sender<()>);

    impl AsyncWrite for Stalled {
        fn poll_write(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            _: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            let _ = self.0.try_send(());
            Poll::Pending
        }

        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn a_blocked_agent_write_does_not_pin_the_pipes_after_the_sdk_drops() {
        let deadline = std::time::Duration::from_secs(5);
        let (mut agent_side_out, agent_out) = tokio::io::duplex(4096);
        let (stalled_tx, mut stalled_rx) = mpsc::channel(1);
        let mut sdk = wrap(agent_out, Stalled(stalled_tx));
        sdk.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":0}\n")
            .await
            .unwrap();
        tokio::time::timeout(deadline, stalled_rx.recv())
            .await
            .expect("the agent write blocks")
            .expect("the writer is alive");
        // The agent never reads stdin; dropping the SDK end must still free both pipes.
        drop(sdk);
        let mut seen = Vec::new();
        let read = tokio::time::timeout(
            deadline,
            tokio::io::AsyncReadExt::read_to_end(&mut agent_side_out, &mut seen),
        )
        .await
        .expect("the stdout pump kept the agent's stdout");
        assert_eq!(read.unwrap(), 0);
        // A re-poll may have queued more signals; the channel closes with the writer.
        tokio::time::timeout(deadline, async {
            while stalled_rx.recv().await.is_some() {}
        })
        .await
        .expect("the write pump kept the agent's stdin");
    }

    #[tokio::test]
    async fn a_standalone_null_id_error_is_not_grouped_into_a_later_batch() {
        let (mut agent_side_out, agent_out) = tokio::io::duplex(4096);
        let (agent_in, agent_side_in) = tokio::io::duplex(4096);
        let (read, write) = tokio::io::split(wrap(agent_out, agent_in));
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        let agent = async move {
            // `[]` is answered with one error; the batch owes its own.
            let input = "[]\n[{\"jsonrpc\":\"2.0\",\"id\":null,\"method\":\"x/unknown\"}]\n";
            agent_side_out.write_all(input.as_bytes()).await.unwrap();
            let mut replies = BufReader::new(agent_side_in).lines();
            let standalone = read_value(&mut replies).await;
            assert_eq!(standalone["error"]["code"], -32600, "{standalone}");
            let grouped = read_value(&mut replies).await;
            let entries = grouped
                .as_array()
                .expect("the batch is answered as one array");
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0]["error"]["code"], -32601, "{grouped}");
            let _ = done_tx.send(());
        };
        let client = Client.v1().connect_with(
            ByteStreams::new(write.compat_write(), read.compat()),
            async move |_cx| {
                // Returning earlier would shut the connection down under the peer.
                let _ = done_rx.await;
                Ok(())
            },
        );
        let (result, ()) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            tokio::join!(client, agent)
        })
        .await
        .expect("both replies arrive");
        result.unwrap();
    }

    #[tokio::test]
    async fn duplicate_request_ids_are_refused_inside_the_batch_reply() {
        let (mut agent_side_out, agent_out) = tokio::io::duplex(4096);
        let (agent_in, agent_side_in) = tokio::io::duplex(4096);
        let (read, write) = tokio::io::split(wrap(agent_out, agent_in));
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        let agent = async move {
            // A duplicate inside a batch, then a batch and a standalone line
            // reusing an id the first batch still waits on.
            let input = concat!(
                r#"[{"jsonrpc":"2.0","id":1,"method":"x/unknown"},{"jsonrpc":"2.0","id":1,"method":"x/unknown"}]"#,
                "\n",
                r#"[{"jsonrpc":"2.0","id":1,"method":"x/unknown"}]"#,
                "\n",
                r#"{"jsonrpc":"2.0","id":1,"method":"x/unknown"}"#,
                "\n",
            );
            agent_side_out.write_all(input.as_bytes()).await.unwrap();
            let mut replies = BufReader::new(agent_side_in).lines();
            let mut seen = Vec::new();
            for _ in 0..3 {
                seen.push(read_value(&mut replies).await);
            }
            let refused = |v: &Value| v["error"]["data"] == "duplicate request id";
            // The refused batch is answered alone, the standalone one bare.
            let lone = seen
                .iter()
                .find(|v| v.as_array().is_some_and(|a| a.len() == 1 && refused(&a[0])))
                .unwrap_or_else(|| panic!("a lone refusal batch in {seen:?}"));
            assert_eq!(lone[0]["id"], 1);
            assert!(seen.iter().any(refused), "bare refusal in {seen:?}");
            // The first batch: its own reply plus the refused duplicate.
            let first = seen
                .iter()
                .find(|v| v.as_array().is_some_and(|a| a.len() == 2))
                .unwrap_or_else(|| panic!("a two-entry batch in {seen:?}"));
            assert_eq!(
                first
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|e| refused(e))
                    .count(),
                1
            );
            let _ = done_tx.send(());
        };
        let client = Client.v1().connect_with(
            ByteStreams::new(write.compat_write(), read.compat()),
            async move |_cx| {
                // Returning earlier would shut the connection down under the peer.
                let _ = done_rx.await;
                Ok(())
            },
        );
        let (result, ()) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            tokio::join!(client, agent)
        })
        .await
        .expect("every request is answered, none hangs");
        result.unwrap();
    }

    #[tokio::test]
    async fn a_flood_of_duplicate_ids_closes_the_transport_instead_of_queueing() {
        let (mut agent_side_out, agent_out) = tokio::io::duplex(1 << 20);
        // Tiny and never read: the write pump blocks on its first refusal.
        let (agent_in, _agent_side_in) = tokio::io::duplex(16);
        let mut sdk = wrap(agent_out, agent_in);
        let request = r#"{"jsonrpc":"2.0","id":1,"method":"x"}"#;
        let flood = format!("{request}\n").repeat(REFUSAL_CAPACITY + 50);
        agent_side_out.write_all(flood.as_bytes()).await.unwrap();
        let mut seen = Vec::new();
        let closed = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            tokio::io::AsyncReadExt::read_to_end(&mut sdk, &mut seen),
        )
        .await;
        assert!(closed.is_ok(), "the transport stayed open under the flood");
    }

    #[tokio::test]
    async fn agent_exit_reaches_the_sdk_as_eof() {
        let (agent_side_out, agent_out) = tokio::io::duplex(4096);
        let (agent_in, _agent_side_in) = tokio::io::duplex(4096);
        let mut sdk = wrap(agent_out, agent_in);
        drop(agent_side_out);
        let mut buf = Vec::new();
        let read = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            tokio::io::AsyncReadExt::read_to_end(&mut sdk, &mut buf),
        )
        .await
        .expect("the SDK end sees EOF");
        assert_eq!(read.unwrap(), 0);
    }

    #[tokio::test]
    async fn a_batched_initialize_response_reaches_a_v1_client() {
        let (mut agent_side_out, agent_out) = tokio::io::duplex(4096);
        let (agent_in, agent_side_in) = tokio::io::duplex(4096);
        let (read, write) = tokio::io::split(wrap(agent_out, agent_in));
        let agent = async move {
            let mut requests = BufReader::new(agent_side_in).lines();
            let request = read_value(&mut requests).await;
            assert_eq!(request["method"], "initialize");
            let reply = serde_json::json!([{
                "jsonrpc": "2.0",
                "id": request["id"],
                "result": {"protocolVersion": 1, "agentCapabilities": {}},
            }]);
            agent_side_out
                .write_all(format!("{reply}\n").as_bytes())
                .await
                .unwrap();
            // Keep the agent end open until the client is done.
            requests.next_line().await.ok();
        };
        let client = Client.v1().connect_with(
            ByteStreams::new(write.compat_write(), read.compat()),
            async |cx| {
                let response = cx
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                assert_eq!(response.protocol_version, ProtocolVersion::V1);
                Ok(())
            },
        );
        let (result, ()) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            tokio::join!(client, agent)
        })
        .await
        .expect("initialize completes");
        result.unwrap();
    }
}
