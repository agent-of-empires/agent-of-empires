//! Seam between daemon features and plugin workers, so callers can be tested with a fake.

use std::time::Duration;

use futures_util::future::BoxFuture;
use serde_json::Value;

use crate::plugin::host::PluginHost;
use crate::plugin::worker_requests::WorkerRequestError;

pub trait WorkerCall: Send + Sync {
    fn call<'a>(
        &'a self,
        plugin_id: &'a str,
        method: &'a str,
        params: Value,
        timeout: Duration,
    ) -> BoxFuture<'a, Result<Value, WorkerRequestError>>;

    /// A fresh envelope id for [`Self::call_as`], unique for the host.
    fn request_id(&self) -> String;

    /// [`Self::call`] with envelope id `id`, so a later notification can name the request.
    /// Waits at most `start_wait` for a worker that is still starting.
    fn call_as<'a>(
        &'a self,
        id: &'a str,
        plugin_id: &'a str,
        method: &'a str,
        params: Value,
        start_wait: Duration,
        timeout: Duration,
    ) -> BoxFuture<'a, Result<Value, WorkerRequestError>>;

    /// Sends a JSON-RPC notification; `false` when the plugin has no running worker.
    fn notify<'a>(
        &'a self,
        plugin_id: &'a str,
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
        plugin_id: &'a str,
        method: &'a str,
        params: Value,
        timeout: Duration,
    ) -> BoxFuture<'a, Result<Value, WorkerRequestError>> {
        Box::pin(self.request_worker(plugin_id, method, params, timeout))
    }

    fn request_id(&self) -> String {
        self.next_request_id()
    }

    fn call_as<'a>(
        &'a self,
        id: &'a str,
        plugin_id: &'a str,
        method: &'a str,
        params: Value,
        start_wait: Duration,
        timeout: Duration,
    ) -> BoxFuture<'a, Result<Value, WorkerRequestError>> {
        Box::pin(self.request_worker_as(id, plugin_id, method, params, start_wait, timeout))
    }

    fn notify<'a>(
        &'a self,
        plugin_id: &'a str,
        method: &'a str,
        params: Value,
    ) -> BoxFuture<'a, bool> {
        Box::pin(self.notify_worker(plugin_id, method, params))
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
