//! Seam between daemon features and plugin workers, so callers can be tested with a fake.

use std::time::Duration;

use futures_util::future::BoxFuture;
use serde_json::Value;

use crate::plugin::host::PluginHost;
use crate::plugin::worker_requests::{CallTicket, WorkerRef, WorkerRequestError};

pub trait WorkerCall: Send + Sync {
    fn call<'a>(
        &'a self,
        worker: WorkerRef<'a>,
        method: &'a str,
        params: Value,
        timeout: Duration,
    ) -> BoxFuture<'a, Result<Value, WorkerRequestError>>;

    /// A fresh ticket for [`Self::call_as`], with an envelope id unique for the host.
    fn new_ticket(&self) -> CallTicket;

    /// [`Self::call`] with the envelope id of `ticket`, so a later notification can name the
    /// request, and the ticket records the worker instance that was handed it. Waits at most
    /// `start_wait` for a worker that is still starting.
    fn call_as<'a>(
        &'a self,
        ticket: &'a CallTicket,
        worker: WorkerRef<'a>,
        method: &'a str,
        params: Value,
        start_wait: Duration,
        timeout: Duration,
    ) -> BoxFuture<'a, Result<Value, WorkerRequestError>>;

    /// Sends a JSON-RPC notification to the worker instance `ticket` was queued to; `false` when the
    /// call was never queued or that instance has ended.
    fn notify_sent<'a>(
        &'a self,
        ticket: &'a CallTicket,
        method: &'a str,
        params: Value,
    ) -> BoxFuture<'a, bool>;

    fn report_failure(&self, plugin_id: &str, message: &str);

    /// Tells the user a plugin's MCP server selection was ignored.
    fn report_serve_failure(&self, plugin_id: &str, message: &str);

    /// Tells the user a plugin's MCP server replaced one of their configured servers.
    fn report_serve_notice(&self, plugin_id: &str, message: &str);
}

impl WorkerCall for PluginHost {
    fn call<'a>(
        &'a self,
        worker: WorkerRef<'a>,
        method: &'a str,
        params: Value,
        timeout: Duration,
    ) -> BoxFuture<'a, Result<Value, WorkerRequestError>> {
        Box::pin(self.request_worker(worker, method, params, timeout))
    }

    fn new_ticket(&self) -> CallTicket {
        CallTicket::new(self.next_request_id())
    }

    fn call_as<'a>(
        &'a self,
        ticket: &'a CallTicket,
        worker: WorkerRef<'a>,
        method: &'a str,
        params: Value,
        start_wait: Duration,
        timeout: Duration,
    ) -> BoxFuture<'a, Result<Value, WorkerRequestError>> {
        Box::pin(self.request_worker_as(ticket, worker, method, params, start_wait, timeout))
    }

    fn notify_sent<'a>(
        &'a self,
        ticket: &'a CallTicket,
        method: &'a str,
        params: Value,
    ) -> BoxFuture<'a, bool> {
        let line = serde_json::json!({ "jsonrpc": "2.0", "method": method, "params": params })
            .to_string()
            + "\n";
        let sent = ticket.queued_to().is_some_and(|tx| tx.send(line).is_ok());
        Box::pin(async move { sent })
    }

    fn report_failure(&self, plugin_id: &str, message: &str) {
        self.notify_host(
            plugin_id,
            crate::plugin::ui_state::Tone::Danger,
            "MCP rewrite hook failed".to_string(),
            Some(message.to_string()),
        );
    }

    fn report_serve_failure(&self, plugin_id: &str, message: &str) {
        self.notify_host(
            plugin_id,
            crate::plugin::ui_state::Tone::Danger,
            "MCP server selection failed".to_string(),
            Some(message.to_string()),
        );
    }

    fn report_serve_notice(&self, plugin_id: &str, message: &str) {
        self.notify_host(
            plugin_id,
            crate::plugin::ui_state::Tone::Warn,
            "MCP server replaced".to_string(),
            Some(message.to_string()),
        );
    }
}
