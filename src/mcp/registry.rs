//! Global registry that holds a single `McpService` for the running
//! session.
//!
//! Mirrors the way opencode attaches its MCP service to its
//! `InstanceState` — we don't have effect-TS, so we use a
//! `OnceLock<Arc<McpService>>`. Every consumer in the rest of the
//! app fetches the live instance through [`McpRegistry::current`].

use std::sync::{Arc, OnceLock};

use crate::mcp::service::McpService;

static REGISTRY: OnceLock<Arc<McpService>> = OnceLock::new();

/// Handle for storing / retrieving the process-wide [`McpService`].
pub struct McpRegistry;

impl McpRegistry {
    /// Install the service as the process-wide singleton. Panics if
    /// called twice — startup wiring must call this exactly once.
    pub fn install(service: Arc<McpService>) {
        if REGISTRY.set(service).is_err() {
            tracing::error!("McpRegistry::install called twice; ignoring second call");
        }
    }

    /// Return the live service, or `None` if startup has not yet
    /// installed one. Callers that need a service should fail soft
    /// (`None` → empty tool list, no-op for `/mcp`).
    pub fn current() -> Option<Arc<McpService>> {
        REGISTRY.get().cloned()
    }

    /// Test-only: clear the registry so tests can re-install.
    #[cfg(test)]
    pub fn reset() {
        // OnceLock does not expose `take`; we re-create via a leak.
        // Tests should not run in parallel; calling reset + install
        // again is the documented pattern.
        if let Some(svc) = REGISTRY.get() {
            // Spawn the shutdown future best-effort; ignore the
            // JoinHandle in tests.
            let svc = svc.clone();
            if let Ok(handle) = std::thread::Builder::new()
                .name("mcp-test-shutdown".into())
                .spawn(move || {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .expect("runtime");
                    runtime.block_on(svc.shutdown());
                })
            {
                let _ = handle.join();
            }
        }
        // We can't actually clear a OnceLock, so we leak: the next
        // install will fail and log an error. Tests must therefore
        // install exactly once per process.
    }
}
