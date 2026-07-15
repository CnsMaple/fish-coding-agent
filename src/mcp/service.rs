//! The `McpService`: a single instance-wide owner of every
//! configured MCP server. Mirrors the shape of opencode's
//! `packages/opencode/src/mcp/index.ts` (the Effect-typed `Service`
//! context tag).
//!
//! In Rust we use `Arc<RwLock<...>>` for the shared state and
//! `tokio::spawn` for the per-server supervisor task. The service
//! is installed once at startup via [`McpRegistry::install`].

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use serde::Serialize;
use thiserror::Error;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;

use crate::mcp::catalog::McpToolSpec;
use crate::mcp::client::{ClientError, McpClientHandle, TransportKind};
use crate::mcp::config::McpServerConfig;
use crate::mcp::status::McpStatus;

use super::auth::McpAuthStore;
use super::registry::McpRegistry;

/// Read-only snapshot of the service's state. Cheap to clone (it
/// holds owned `String`s and `serde_json::Value`s, not the live
/// clients).
#[derive(Debug, Clone, Default, Serialize)]
pub struct StateSnapshot {
    /// Per-server config (resolved; toggle entries are absent).
    pub config: HashMap<String, McpServerConfig>,
    /// Per-server live status.
    pub status: HashMap<String, McpStatus>,
    /// Per-server cached tool list, keyed by `<server>_<tool>`.
    pub tools: HashMap<String, McpToolSpec>,
}

pub struct McpService {
    inner: Arc<RwLock<ServiceState>>,
    cwd: PathBuf,
    /// Backstop of the supervisor task. Kept so we can join /
    /// cancel it on shutdown.
    _supervisor: Mutex<Option<JoinHandle<()>>>,
    /// Per-client supervisor tasks (one per connected server). The
    /// key is the server name.
    clients: RwLock<HashMap<String, ClientSlot>>,
    auth: McpAuthStore,
    /// Bounded event channel for sending `AppMsg`-equivalent
    /// notifications into the rest of the app. The app wires this
    /// up by calling [`McpService::bind_event_sink`]. Wrapped in
    /// an `Arc` so the per-client watcher tasks can hold their own
    /// reference cheaply.
    /// Uses `std::sync::Mutex` (not `tokio::sync::Mutex`) because
    /// it's only locked briefly during clone_weak and never held
    /// across an await point.
    event_sink: std::sync::Mutex<Option<Arc<dyn McpEventSink>>>,
}

/// Per-server runtime state. The supervisor task owns the live
/// client; the rest of the state is read-only from the public API.
#[allow(dead_code)]
struct ClientSlot {
    pub name: String,
    pub kind: TransportKind,
    pub pid: Option<u32>,
    pub handle: McpClientHandle,
    /// Background task that watches `client.is_closed()` and
    /// surfaces `ClientClosed` events.
    pub _watcher: JoinHandle<()>,
}

use tokio::sync::Mutex;

impl McpService {
    /// Construct a service and start supervisor tasks for every
    /// enabled server in `cfg`. Returns once the `initialize`
    /// handshake has been attempted for each server — but the
    /// returned service is *not* done connecting on slow servers;
    /// the background supervisor task continues to track them.
    pub async fn init_from_config(
        cfg: &HashMap<String, super::config::McpEntry>,
        cwd: PathBuf,
    ) -> Arc<Self> {
        let auth = McpAuthStore::load_or_default();
        let svc = Arc::new(Self {
            inner: Arc::new(RwLock::new(ServiceState::default())),
            cwd,
            _supervisor: Mutex::new(None),
            clients: RwLock::new(HashMap::new()),
            auth,
            event_sink: std::sync::Mutex::new(None),
        });
        // Seed the config map.
        {
            let mut state = svc.inner.write().await;
            for (name, entry) in cfg {
                if let Some(cfg) = entry.as_config() {
                    state.config.insert(name.clone(), cfg.clone());
                    state.status.insert(name.clone(), McpStatus::Disabled);
                }
            }
        }
        // Install the registry immediately so the TUI can access
        // the service without waiting for connects to finish.
        McpRegistry::install(svc.clone());
        // Fire-and-forget: spawn one connect task per enabled server.
        // The TUI renders right away with `Disabled` status; the
        // background tasks update the status to `Connected` / `Failed`
        // and emit `McpStatusChanged` events when done.
        for (name, entry) in cfg {
            let Some(server_cfg) = entry.as_config() else {
                continue;
            };
            if !server_cfg.enabled() {
                continue;
            }
            let svc2 = svc.clone();
            let name = name.clone();
            let server_cfg = server_cfg.clone();
            tokio::spawn(async move {
                svc2.connect(&name, &server_cfg).await;
            });
        }
        svc
    }

    /// Attach a callback that receives MCP lifecycle events
    /// (status changes, tool list updates, auth required). The
    /// app installs one that translates these into `AppMsg`s.
    pub async fn bind_event_sink(&self, sink: Arc<dyn McpEventSink>) {
        *self.event_sink.lock().unwrap() = Some(sink);
    }

    /// Cheap read-only snapshot of the current state.
    pub async fn snapshot(&self) -> StateSnapshot {
        let state = self.inner.read().await;
        StateSnapshot {
            config: state.config.clone(),
            status: state.status.clone(),
            tools: state.tools.clone(),
        }
    }

    /// Synchronous snapshot — returns `Err` if the write lock is
    /// held. Used by the picker which must not block the UI thread.
    pub fn try_snapshot(&self) -> Result<StateSnapshot, tokio::sync::TryLockError> {
        let state = self.inner.try_read()?;
        Ok(StateSnapshot {
            config: state.config.clone(),
            status: state.status.clone(),
            tools: state.tools.clone(),
        })
    }

    /// List the configured servers, in declaration order.
    pub async fn configured_names(&self) -> Vec<String> {
        let state = self.inner.read().await;
        let mut names: Vec<String> = state.config.keys().cloned().collect();
        names.sort();
        names
    }

    /// Synchronous version of [`Self::configured_names`]. Used by
    /// the picker which can't await.
    pub fn configured_names_sync(&self) -> Result<Vec<String>, tokio::sync::TryLockError> {
        let state = self.inner.try_read()?;
        let mut names: Vec<String> = state.config.keys().cloned().collect();
        names.sort();
        Ok(names)
    }

    /// Status of a single server.
    pub async fn status_of(&self, name: &str) -> McpStatus {
        let state = self.inner.read().await;
        state
            .status
            .get(name)
            .cloned()
            .unwrap_or(McpStatus::Disabled)
    }

    /// Sync version of [`Self::status_of`]. Returns `Disabled` if
    /// the lock is held by another task.
    pub fn status_of_sync(&self, name: &str) -> Result<McpStatus, tokio::sync::TryLockError> {
        let state = self.inner.try_read()?;
        Ok(state
            .status
            .get(name)
            .cloned()
            .unwrap_or(McpStatus::Disabled))
    }

    /// All tools, keyed by `<server>_<tool>`.
    pub async fn list_tools(&self) -> HashMap<String, McpToolSpec> {
        self.snapshot().await.tools
    }

    /// Resolve a tool key (`<server>_<tool>`) to its spec, if any.
    pub async fn lookup_tool(&self, key: &str) -> Option<McpToolSpec> {
        self.inner.read().await.tools.get(key).cloned()
    }

    /// Invoke a tool by its combined key. Returns the rendered
    /// text result. Errors include the tool's `is_error` flag.
    pub async fn call_tool(
        &self,
        key: &str,
        arguments: serde_json::Value,
    ) -> Result<String, ServiceError> {
        let spec = self
            .lookup_tool(key)
            .await
            .ok_or_else(|| ServiceError::NotFound(key.to_string()))?;
        let clients = self.clients.read().await;
        let slot = clients
            .get(&spec.server)
            .ok_or_else(|| ServiceError::NotFound(spec.server.clone()))?;
        let result = slot
            .handle
            .call_tool(&spec.name, Some(arguments))
            .await
            .map_err(ServiceError::from)?;
        Ok(McpClientHandle::render_text(&result))
    }

    /// Connect (or reconnect) a single server. Updates the status
    /// map; fires the event sink on completion.
    pub async fn connect(&self, name: &str, cfg: &McpServerConfig) {
        self.set_status(name, McpStatus::Disabled).await;
        let result = super::client::connect(name, cfg, &self.cwd, &self.auth).await;
        match result {
            Ok(handle) => {
                let tools = match handle.list_tools().await {
                    Ok(tools) => tools,
                    Err(e) => {
                        self.set_status(
                            name,
                            McpStatus::Failed {
                                error: format!("tools/list failed: {e}"),
                            },
                        )
                        .await;
                        handle.close().await;
                        return;
                    }
                };
                {
                    let mut state = self.inner.write().await;
                    state.config.insert(name.to_string(), cfg.clone());
                    for tool in &tools {
                        state.tools.insert(tool.key.clone(), tool.clone());
                    }
                }
                let kind = handle.kind;
                let pid = handle.pid;
                let watcher = tokio::spawn({
                    let svc = self.clone_weak();
                    let name = name.to_string();
                    async move {
                        // Best-effort watcher: poll the transport
                        // for closure and surface an event.
                        svc.watch_close(&name).await;
                    }
                });
                {
                    let mut clients = self.clients.write().await;
                    clients.insert(
                        name.to_string(),
                        ClientSlot {
                            name: name.to_string(),
                            kind,
                            pid,
                            handle,
                            _watcher: watcher,
                        },
                    );
                }
                self.set_status(name, McpStatus::Connected).await;
                self.emit(McpEvent::ToolsChanged {
                    server: name.to_string(),
                })
                .await;
            }
            Err(ClientError::Unauthorized(err)) => {
                self.set_status(name, McpStatus::NeedsAuth).await;
                self.emit(McpEvent::AuthRequired {
                    server: name.to_string(),
                    error: err,
                })
                .await;
            }
            Err(ClientError::Spawn(err)) => {
                self.set_status(
                    name,
                    McpStatus::Failed {
                        error: format!("spawn failed: {err}"),
                    },
                )
                .await;
            }
            Err(err) => {
                self.set_status(
                    name,
                    McpStatus::Failed {
                        error: err.to_string(),
                    },
                )
                .await;
            }
        }
    }

    /// Disconnect a server (status becomes Disabled, client is
    /// cancelled, tools for that server are dropped).
    pub async fn disconnect(&self, name: &str) {
        let mut clients = self.clients.write().await;
        if let Some(slot) = clients.remove(name) {
            slot.handle.close().await;
        }
        drop(clients);
        let mut state = self.inner.write().await;
        state
            .tools
            .retain(|k, _| !k.starts_with(&format!("{name}_")));
        state.status.insert(name.to_string(), McpStatus::Disabled);
    }

    /// Cancel every active client and tear down the service.
    pub async fn shutdown(self: Arc<Self>) {
        let mut clients = self.clients.write().await;
        for (_, slot) in clients.drain() {
            slot.handle.close().await;
        }
    }

    /// Background watcher: re-list tools when the server tells us
    /// the tool list changed.
    async fn watch_close(&self, name: &str) {
        // We don't have a fine-grained close hook; poll the
        // transport's `is_closed` flag (rmcp exposes it on the
        // peer) and re-list every 2s as a fallback.
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            let clients = self.clients.read().await;
            let Some(slot) = clients.get(name) else {
                return;
            };
            if slot.handle.peer.is_transport_closed() {
                drop(clients);
                self.disconnect(name).await;
                self.emit(McpEvent::ClientClosed {
                    server: name.to_string(),
                })
                .await;
                return;
            }
        }
    }

    async fn set_status(&self, name: &str, status: McpStatus) {
        let mut state = self.inner.write().await;
        let changed = state
            .status
            .get(name)
            .map(|old| old != &status)
            .unwrap_or(true);
        state.status.insert(name.to_string(), status.clone());
        drop(state);
        if changed {
            self.emit(McpEvent::StatusChanged {
                name: name.to_string(),
                status,
            })
            .await;
        }
    }

    async fn emit(&self, event: McpEvent) {
        let sink = self.event_sink.lock().unwrap();
        if let Some(s) = sink.as_ref() {
            s.emit(event);
        }
    }

    #[allow(dead_code)]
    fn emit_sync(&self, event: McpEvent) {
        if let Ok(guard) = self.event_sink.lock() {
            if let Some(sink) = guard.as_ref() {
                sink.emit(event);
            }
        }
    }

    /// Create a "weak" clone — a fresh `Arc<Self>` that shares
    /// the underlying state with the original service. Used by
    /// the per-client watcher tasks so the supervisor can keep
    /// a strong reference while the spawned task holds its own.
    fn clone_weak(&self) -> Arc<Self> {
        let sink = self.event_sink.lock().unwrap().clone();
        Arc::new(Self {
            inner: self.inner.clone(),
            cwd: self.cwd.clone(),
            _supervisor: <Mutex<Option<JoinHandle<()>>>>::new(None),
            clients: RwLock::new(HashMap::new()),
            auth: self.auth.clone(),
            event_sink: std::sync::Mutex::new(sink),
        })
    }
}

/// Mutated shared state. Held behind an `RwLock` in [`McpService`].
#[derive(Default)]
struct ServiceState {
    config: HashMap<String, McpServerConfig>,
    status: HashMap<String, McpStatus>,
    tools: HashMap<String, McpToolSpec>,
}

/// Events emitted by [`McpService`]. The app translates these
/// into `AppMsg`s via a user-supplied [`McpEventSink`].
#[derive(Debug, Clone)]
pub enum McpEvent {
    ToolsChanged { server: String },
    StatusChanged { name: String, status: McpStatus },
    AuthRequired { server: String, error: String },
    ClientClosed { server: String },
}

/// Callback trait used by [`McpService`] to publish lifecycle
/// events. The app installs one of these so the TUI can react
/// (e.g. re-aggregate tool specs, surface a toast).
pub trait McpEventSink: Send + Sync + 'static {
    fn emit(&self, event: McpEvent);
}

#[derive(Debug, Error)]
pub enum ServiceError {
    #[error("mcp server `{0}` is not configured")]
    NotFound(String),
    #[error("mcp transport error: {0}")]
    Transport(String),
    #[error("mcp protocol error: {0}")]
    Protocol(String),
    #[error("mcp auth error: {0}")]
    Auth(String),
    #[error("mcp config error: {0}")]
    Config(String),
}

impl From<ClientError> for ServiceError {
    fn from(e: ClientError) -> Self {
        match e {
            ClientError::Unauthorized(m) => ServiceError::Auth(m),
            ClientError::Spawn(m) => ServiceError::Config(m),
            ClientError::Transport(m) => ServiceError::Transport(m),
            ClientError::Protocol(m) => ServiceError::Protocol(m),
            ClientError::Io(e) => ServiceError::Transport(e.to_string()),
        }
    }
}
