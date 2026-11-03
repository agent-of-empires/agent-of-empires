//! JSON-RPC batch support for direct agent stdio. SDK 5 rejects every array,
//! so incoming batches are split into single lines and the replies to their
//! requests are regrouped into one array line.

use agent_client_protocol::{RawJsonRpcMessage, TransportFrame};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
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
    /// Oldest first: replies with a null id go to the oldest batch awaiting one.
    open: BTreeMap<u64, Open>,
}

struct Open {
    named: usize,
    anonymous: usize,
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
    // The agent's stdout ended: the SDK must see EOF, not a quiet pipe.
    let _ = to_sdk.shutdown().await;
}

/// What the SDK answers for one batch entry. Entries it cannot attribute to an
/// id (malformed, non-objects, null-id requests) get an `id: null` error.
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

fn track_requests(state: &Shared, items: &[Value]) {
    let mut named = Vec::new();
    let mut anonymous = 0;
    for item in items {
        match expected_reply(item) {
            Expect::Named(id) => named.push(id),
            Expect::Anonymous => anonymous += 1,
            Expect::Nothing => {}
        }
    }
    if named.is_empty() && anonymous == 0 {
        return;
    }
    let mut batches = state.lock().expect("batch state poisoned");
    let batch = batches.next;
    batches.next += 1;
    batches.open.insert(
        batch,
        Open {
            named: named.len(),
            anonymous,
            replies: Vec::new(),
        },
    );
    for id in named {
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
    let mut batches = state.lock().expect("batch state poisoned");
    let batch = match reply.get("id") {
        None => return Claimed::Single,
        // ponytail: a null-id reply to a non-batch line sent while a batch
        // awaits one can be grouped into that batch.
        Some(id) if id.is_null() => {
            let Some((&batch, _)) = batches.open.iter().find(|(_, open)| open.anonymous > 0) else {
                return Claimed::Single;
            };
            batches.open.get_mut(&batch).expect("found").anonymous -= 1;
            batch
        }
        Some(id) => {
            let Some(batch) = batches.owner.remove(&id.to_string()) else {
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
        };
        let client = Client.v1().connect_with(
            ByteStreams::new(write.compat_write(), read.compat()),
            async |_cx| {
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
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
