//! Host-initiated calls to a plugin worker's declared `provider.<name>` methods.

use std::time::Duration;

use aoe_plugin_api::{PluginManifest, PROVIDER_CALL_METHOD_PREFIX};
use serde_json::Value;

use crate::daemon::ApiErrorCode;
use crate::plugin::worker_call::WorkerCall;
use crate::plugin::worker_requests::{WorkerRef, WorkerRequestError};

#[derive(Debug)]
pub enum ProviderCallError {
    /// No enabled, granted plugin has this id.
    NotActive,
    /// The active manifest does not declare this call.
    Undeclared,
    RequestTooLarge,
    ResponseTooLarge,
    Worker(WorkerRequestError),
}

impl std::fmt::Display for ProviderCallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotActive => f.write_str("plugin is not active"),
            Self::Undeclared => f.write_str("plugin does not declare this provider call"),
            Self::RequestTooLarge => f.write_str("request params exceed the call's max_bytes"),
            Self::ResponseTooLarge => f.write_str("worker reply exceeds the call's max_bytes"),
            Self::Worker(e) => e.fmt(f),
        }
    }
}

impl ProviderCallError {
    pub fn api_code(&self) -> ApiErrorCode {
        match self {
            Self::NotActive => ApiErrorCode::ProviderPluginInactive,
            Self::Undeclared => ApiErrorCode::ProviderCallUndeclared,
            Self::RequestTooLarge => ApiErrorCode::ProviderRequestTooLarge,
            Self::ResponseTooLarge => ApiErrorCode::ProviderResponseTooLarge,
            Self::Worker(WorkerRequestError::NoWorker) => ApiErrorCode::ProviderNoWorker,
            Self::Worker(WorkerRequestError::Timeout) => ApiErrorCode::ProviderTimeout,
            Self::Worker(WorkerRequestError::WorkerGone) => ApiErrorCode::ProviderWorkerGone,
            Self::Worker(WorkerRequestError::Superseded) => ApiErrorCode::ProviderSuperseded,
            Self::Worker(WorkerRequestError::MethodNotFound) => {
                ApiErrorCode::ProviderMethodNotFound
            }
            Self::Worker(WorkerRequestError::Rejected) => ApiErrorCode::ProviderRejected,
        }
    }
}

/// Calls `provider.<name>` on the active plugin `plugin_id`, found in the registry.
pub async fn call_active(
    caller: &dyn WorkerCall,
    plugin_id: &str,
    name: &str,
    params: Value,
) -> Result<Value, ProviderCallError> {
    let registry = crate::plugin::registry();
    let active = registry
        .active()
        .find(|p| p.id() == plugin_id)
        .map(|p| (&p.manifest, p.manifest_hash.as_str()));
    call(caller, plugin_id, active, name, params).await
}

/// `active` is the plugin's manifest and hash when it is enabled and granted. The host builds
/// the method, timeout and size cap from the declaration; the caller supplies only the name and
/// params. The worker's stdout reader is unbounded by design (honest-worker model), so the
/// reply cap is checked after the read.
pub async fn call(
    caller: &dyn WorkerCall,
    plugin_id: &str,
    active: Option<(&PluginManifest, &str)>,
    name: &str,
    params: Value,
) -> Result<Value, ProviderCallError> {
    let (manifest, manifest_hash) = active.ok_or(ProviderCallError::NotActive)?;
    let decl = manifest
        .provider_calls
        .iter()
        .find(|c| c.name == name)
        .ok_or(ProviderCallError::Undeclared)?;
    if serialized_len(&params) > decl.max_bytes {
        return Err(ProviderCallError::RequestTooLarge);
    }
    let method = format!("{PROVIDER_CALL_METHOD_PREFIX}{name}");
    let reply = caller
        .call(
            WorkerRef {
                plugin_id,
                manifest_hash,
            },
            &method,
            params,
            Duration::from_millis(decl.timeout_ms),
        )
        .await
        .map_err(ProviderCallError::Worker)?;
    if serialized_len(&reply) > decl.max_bytes {
        return Err(ProviderCallError::ResponseTooLarge);
    }
    Ok(reply)
}

fn serialized_len(value: &Value) -> usize {
    serde_json::to_vec(value).map_or(usize::MAX, |v| v.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::worker_requests::CallTicket;
    use futures_util::future::BoxFuture;
    use serde_json::json;
    use std::sync::Mutex;

    type Reply = Result<Value, WorkerRequestError>;

    struct Fake {
        reply: Mutex<Option<Reply>>,
        calls: Mutex<Vec<(String, String, String, Value, Duration)>>,
    }

    impl Fake {
        fn replying(reply: Reply) -> Self {
            Self {
                reply: Mutex::new(Some(reply)),
                calls: Mutex::default(),
            }
        }
    }

    impl WorkerCall for Fake {
        fn call<'a>(
            &'a self,
            worker: WorkerRef<'a>,
            method: &'a str,
            params: Value,
            timeout: Duration,
        ) -> BoxFuture<'a, Reply> {
            self.calls.lock().unwrap().push((
                worker.plugin_id.into(),
                worker.manifest_hash.into(),
                method.into(),
                params,
                timeout,
            ));
            let reply = self.reply.lock().unwrap().take().expect("one call");
            Box::pin(async move { reply })
        }

        fn new_ticket(&self) -> CallTicket {
            unreachable!()
        }

        fn call_as<'a>(
            &'a self,
            _: &'a CallTicket,
            _: WorkerRef<'a>,
            _: &'a str,
            _: Value,
            _: Duration,
            _: Duration,
        ) -> BoxFuture<'a, Reply> {
            unreachable!()
        }

        fn notify_sent<'a>(
            &'a self,
            _: &'a CallTicket,
            _: &'a str,
            _: Value,
        ) -> BoxFuture<'a, bool> {
            unreachable!()
        }

        fn report_failure(&self, _: &str, _: &str) {
            unreachable!()
        }

        fn report_serve_failure(&self, _: &str, _: &str) {
            unreachable!()
        }

        fn report_serve_notice(&self, _: &str, _: &str) {
            unreachable!()
        }
    }

    fn manifest() -> PluginManifest {
        PluginManifest::from_toml_str(
            "id = \"acme.p\"\nname = \"P\"\nversion = \"1.0.0\"\napi_version = 29\n\
             capabilities = [\"runtime.worker\"]\n\n\
             [runtime]\nkind = \"command\"\ncommand = [\"bin/w\"]\n\n\
             [[provider_calls]]\nname = \"status\"\ntimeout_ms = 1234\nmax_bytes = 16\n",
        )
        .unwrap()
    }

    async fn run(
        fake: &Fake,
        active: bool,
        name: &str,
        params: Value,
    ) -> Result<Value, ProviderCallError> {
        let m = manifest();
        call(
            fake,
            "acme.p",
            active.then_some((&m, "sha256:h")),
            name,
            params,
        )
        .await
    }

    #[tokio::test]
    async fn dispatches_the_declared_method_with_the_declared_timeout() {
        let fake = Fake::replying(Ok(json!({"ok": 1})));
        let out = run(&fake, true, "status", json!({"a": 1})).await.unwrap();
        assert_eq!(out, json!({"ok": 1}));
        let calls = fake.calls.lock().unwrap();
        assert_eq!(
            calls.as_slice(),
            [(
                "acme.p".to_string(),
                "sha256:h".to_string(),
                "provider.status".to_string(),
                json!({"a": 1}),
                Duration::from_millis(1234)
            )]
        );
    }

    #[tokio::test]
    async fn refused_calls_never_reach_the_worker() {
        let cases = [
            (false, "status", json!({})),
            (true, "other", json!({})),
            (true, "status", json!({"padding": "xxxxxxxxxxxxxxxxxxxx"})),
        ];
        let mut errors = Vec::new();
        for (active, name, params) in cases {
            let fake = Fake::replying(Ok(json!(null)));
            errors.push(run(&fake, active, name, params).await.unwrap_err());
            assert!(fake.calls.lock().unwrap().is_empty());
        }
        assert!(matches!(errors[0], ProviderCallError::NotActive));
        assert!(matches!(errors[1], ProviderCallError::Undeclared));
        assert!(matches!(errors[2], ProviderCallError::RequestTooLarge));
    }

    #[tokio::test]
    async fn an_oversize_reply_is_refused_and_an_exact_fit_is_returned() {
        let fake = Fake::replying(Ok(json!("x".repeat(15))));
        assert!(matches!(
            run(&fake, true, "status", json!({})).await.unwrap_err(),
            ProviderCallError::ResponseTooLarge
        ));
        // 16 bytes serialized: 14 characters plus the quotes.
        let fake = Fake::replying(Ok(json!("x".repeat(14))));
        assert!(run(&fake, true, "status", json!({})).await.is_ok());
    }

    #[tokio::test]
    async fn worker_errors_are_preserved() {
        type Make = fn() -> WorkerRequestError;
        let cases: [(Make, &str); 6] = [
            (|| WorkerRequestError::NoWorker, "NoWorker"),
            (|| WorkerRequestError::Timeout, "Timeout"),
            (|| WorkerRequestError::WorkerGone, "WorkerGone"),
            (|| WorkerRequestError::Superseded, "Superseded"),
            (|| WorkerRequestError::MethodNotFound, "MethodNotFound"),
            (|| WorkerRequestError::Rejected, "Rejected"),
        ];
        for (make, label) in cases {
            let fake = Fake::replying(Err(make()));
            match run(&fake, true, "status", json!({})).await.unwrap_err() {
                ProviderCallError::Worker(e) => assert_eq!(format!("{e:?}"), label),
                other => panic!("{label}: {other}"),
            }
        }
    }
}
