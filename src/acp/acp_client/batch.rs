//! JSON-RPC batch support for direct agent stdio. SDK 5 rejects every array,
//! so incoming batches are split into single lines and the replies to their
//! requests are regrouped into one array line.

use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::io::{
    AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, DuplexStream, ReadHalf,
    WriteHalf,
};

#[derive(Default)]
struct Batches {
    next: u64,
    /// Request id (as JSON text) to the batch awaiting its reply.
    owner: HashMap<String, u64>,
    open: HashMap<u64, Open>,
}

struct Open {
    missing: usize,
    replies: Vec<Value>,
}

type Shared = Arc<Mutex<Batches>>;

/// Sits between the agent's stdio and the SDK. Returns the end the SDK reads
/// and writes.
pub(super) fn wrap<R, W>(agent_out: R, agent_in: W) -> DuplexStream
where
    R: AsyncRead + Send + Unpin + 'static,
    W: AsyncWrite + Send + Unpin + 'static,
{
    let (ours, sdk) = tokio::io::duplex(64 * 1024);
    let (from_sdk, to_sdk) = tokio::io::split(ours);
    let state = Shared::default();
    tokio::spawn(split_incoming(agent_out, to_sdk, state.clone()));
    tokio::spawn(group_outgoing(from_sdk, agent_in, state));
    sdk
}

async fn split_incoming<R: AsyncRead + Unpin>(
    agent_out: R,
    mut to_sdk: WriteHalf<DuplexStream>,
    state: Shared,
) {
    let mut lines = BufReader::new(agent_out).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let singles = match serde_json::from_str::<Value>(line.trim()) {
            Ok(Value::Array(items)) if !items.is_empty() => {
                track_requests(&state, &items);
                items.iter().map(Value::to_string).collect()
            }
            // An empty array is not a batch; the SDK answers it with one error.
            _ => vec![line],
        };
        for single in singles {
            if to_sdk
                .write_all(format!("{single}\n").as_bytes())
                .await
                .is_err()
            {
                return;
            }
        }
    }
}

fn track_requests(state: &Shared, items: &[Value]) {
    let ids: Vec<String> = items
        .iter()
        .filter(|item| item.get("method").is_some_and(Value::is_string))
        .filter_map(|item| item.get("id").filter(|id| !id.is_null()))
        .map(Value::to_string)
        .collect();
    if ids.is_empty() {
        return;
    }
    let mut batches = state.lock().expect("batch state poisoned");
    let batch = batches.next;
    batches.next += 1;
    batches.open.insert(
        batch,
        Open {
            missing: ids.len(),
            replies: Vec::new(),
        },
    );
    for id in ids {
        batches.owner.insert(id, batch);
    }
}

async fn group_outgoing<W: AsyncWrite + Unpin>(
    from_sdk: ReadHalf<DuplexStream>,
    mut agent_in: W,
    state: Shared,
) {
    let mut lines = BufReader::new(from_sdk).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let out = match serde_json::from_str::<Value>(line.trim()) {
            Ok(reply) => match claim_reply(&state, reply) {
                Claimed::Held => continue,
                Claimed::Complete(replies) => Value::Array(replies).to_string(),
                Claimed::Single => line,
            },
            Err(_) => line,
        };
        if agent_in
            .write_all(format!("{out}\n").as_bytes())
            .await
            .is_err()
        {
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
    let Some(id) = reply.get("id").map(Value::to_string) else {
        return Claimed::Single;
    };
    let mut batches = state.lock().expect("batch state poisoned");
    let Some(batch) = batches.owner.remove(&id) else {
        return Claimed::Single;
    };
    let open = batches.open.get_mut(&batch).expect("owned batch is open");
    open.replies.push(reply);
    open.missing -= 1;
    if open.missing > 0 {
        return Claimed::Held;
    }
    let done = batches.open.remove(&batch).expect("owned batch is open");
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
