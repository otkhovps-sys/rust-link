//! One plugin process: launch, JSON-RPC 2.0 over newline-delimited stdin/stdout, and
//! restarts with backoff when it exits.

use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        atomic::{AtomicU32, AtomicU64, Ordering},
        Arc, Mutex, RwLock,
    },
    time::Duration,
};

use futures_util::future::BoxFuture;
use serde::Serialize;
use serde_json::{json, Value};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{ChildStdout, Command},
    sync::{mpsc, oneshot, watch},
    time::Instant,
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use super::Manifest;

/// How a plugin is started.
#[derive(Debug, Clone)]
pub struct LaunchSpec {
    pub name: String,
    pub command: Vec<String>,
    pub working_dir: PathBuf,
    pub env: BTreeMap<String, String>,
    /// Settings handed over in `initialize`.
    pub config: Value,
}

impl LaunchSpec {
    /// A relative program path (`./main.py`, `bin/plugin`) is taken from the working
    /// directory; a bare name (`python3`, `node`) is looked up in `PATH`.
    fn program(&self) -> PathBuf {
        let program = Path::new(&self.command[0]);
        let has_dir = self.command[0].contains('/') || self.command[0].contains('\\');
        if has_dir && program.is_relative() {
            self.working_dir.join(program)
        } else {
            program.to_path_buf()
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PluginError {
    #[error("plugin {0} is not running")]
    NotRunning(String),
    #[error("plugin {plugin} did not answer {method} within {secs} s")]
    Timeout { plugin: String, method: String, secs: u64 },
    #[error("plugin {plugin}: {message}")]
    Remote { plugin: String, code: i64, message: String },
    #[error("plugin {0} stopped while handling the request")]
    Closed(String),
}

/// Handles the requests a plugin sends to the node: `(plugin, method, params)`.
pub type InboundHandler =
    Arc<dyn Fn(String, String, Value) -> BoxFuture<'static, Result<Value, (i64, String)>> + Send + Sync>;

pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
const OUTGOING_QUEUE: usize = 1024;
const MAX_BACKOFF: Duration = Duration::from_secs(30);
const STABLE_AFTER: Duration = Duration::from_secs(60);
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum PluginState {
    Starting,
    Running,
    Restarting,
    Failed,
    Stopped,
}

type Pending = oneshot::Sender<Result<Value, PluginError>>;

struct Connection {
    plugin: String,
    outgoing: mpsc::Sender<String>,
    pending: Mutex<HashMap<u64, Pending>>,
    next_id: AtomicU64,
}

impl Connection {
    async fn call(&self, method: &str, params: Value, timeout: Duration) -> Result<Value, PluginError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let line = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string();
        if self.outgoing.send(line).await.is_err() {
            self.pending.lock().unwrap().remove(&id);
            return Err(PluginError::Closed(self.plugin.clone()));
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(PluginError::Closed(self.plugin.clone())),
            Err(_) => {
                self.pending.lock().unwrap().remove(&id);
                Err(PluginError::Timeout {
                    plugin: self.plugin.clone(),
                    method: method.to_string(),
                    secs: timeout.as_secs(),
                })
            }
        }
    }

    /// Fire and forget; dropped when the plugin is not keeping up with its stdin.
    fn notify(&self, method: &str, params: Value) {
        let line = json!({"jsonrpc": "2.0", "method": method, "params": params}).to_string();
        if self.outgoing.try_send(line).is_err() {
            debug!(plugin = %self.plugin, method, "plugin input queue full, notification dropped");
        }
    }

    fn fail_pending(&self) {
        for (_, tx) in self.pending.lock().unwrap().drain() {
            let _ = tx.send(Err(PluginError::Closed(self.plugin.clone())));
        }
    }
}

enum Exit {
    Shutdown,
    /// The program cannot be started at all: a configuration problem, not retried.
    SpawnFailed(String),
    Crashed(String),
}

pub struct PluginProcess {
    pub spec: LaunchSpec,
    connection: RwLock<Option<Arc<Connection>>>,
    /// Last successful handshake; kept while the plugin restarts.
    manifest: RwLock<Option<Arc<Manifest>>>,
    state: watch::Sender<PluginState>,
    restarts: AtomicU32,
    last_error: Mutex<Option<String>>,
}

impl PluginProcess {
    pub fn new(spec: LaunchSpec) -> Arc<Self> {
        Arc::new(Self {
            spec,
            connection: RwLock::new(None),
            manifest: RwLock::new(None),
            state: watch::Sender::new(PluginState::Starting),
            restarts: AtomicU32::new(0),
            last_error: Mutex::new(None),
        })
    }

    pub fn name(&self) -> &str {
        &self.spec.name
    }

    pub fn manifest(&self) -> Option<Arc<Manifest>> {
        self.manifest.read().unwrap().clone()
    }

    pub fn state(&self) -> PluginState {
        *self.state.borrow()
    }

    pub fn restarts(&self) -> u32 {
        self.restarts.load(Ordering::Relaxed)
    }

    pub fn last_error(&self) -> Option<String> {
        self.last_error.lock().unwrap().clone()
    }

    /// Resolves once the first start attempt has finished (running or not).
    pub async fn wait_started(&self) {
        let mut rx = self.state.subscribe();
        let _ = rx.wait_for(|s| *s != PluginState::Starting).await;
    }

    fn connection(&self) -> Option<Arc<Connection>> {
        self.connection.read().unwrap().clone()
    }

    pub async fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value, PluginError> {
        let connection = self
            .connection()
            .ok_or_else(|| PluginError::NotRunning(self.spec.name.clone()))?;
        connection.call(method, params, timeout).await
    }

    pub fn notify(&self, method: &str, params: Value) {
        if let Some(connection) = self.connection() {
            connection.notify(method, params);
        }
    }

    pub fn spawn_supervisor(
        self: &Arc<Self>,
        handler: InboundHandler,
        init_params: Value,
        startup_timeout: Duration,
        shutdown: CancellationToken,
    ) {
        tokio::spawn(Arc::clone(self).supervise(handler, init_params, startup_timeout, shutdown));
    }

    async fn supervise(
        self: Arc<Self>,
        handler: InboundHandler,
        init_params: Value,
        startup_timeout: Duration,
        shutdown: CancellationToken,
    ) {
        let mut backoff = Duration::from_secs(1);
        loop {
            let started = Instant::now();
            let exit = self.run_once(&handler, &init_params, startup_timeout, &shutdown).await;
            let reason = match exit {
                Exit::Shutdown => {
                    self.state.send_replace(PluginState::Stopped);
                    return;
                }
                Exit::SpawnFailed(reason) => {
                    error!(plugin = %self.spec.name, error = %reason, "plugin cannot be started");
                    *self.last_error.lock().unwrap() = Some(reason);
                    self.state.send_replace(PluginState::Failed);
                    return;
                }
                Exit::Crashed(reason) => reason,
            };
            if started.elapsed() >= STABLE_AFTER {
                backoff = Duration::from_secs(1);
            }
            warn!(
                plugin = %self.spec.name,
                error = %reason,
                retry_in_secs = backoff.as_secs(),
                "plugin stopped, restarting"
            );
            *self.last_error.lock().unwrap() = Some(reason);
            self.state.send_replace(PluginState::Restarting);
            tokio::select! {
                _ = shutdown.cancelled() => {
                    self.state.send_replace(PluginState::Stopped);
                    return;
                }
                _ = tokio::time::sleep(backoff) => {}
            }
            backoff = (backoff * 2).min(MAX_BACKOFF);
            self.restarts.fetch_add(1, Ordering::Relaxed);
        }
    }

    async fn run_once(
        &self,
        handler: &InboundHandler,
        init_params: &Value,
        startup_timeout: Duration,
        shutdown: &CancellationToken,
    ) -> Exit {
        let spec = &self.spec;
        let mut command = Command::new(spec.program());
        command
            .args(&spec.command[1..])
            .current_dir(&spec.working_dir)
            .env("RUSTLINK_PLUGIN", &spec.name)
            .env("RUSTLINK_VERSION", env!("CARGO_PKG_VERSION"))
            // Python buffers a piped stdout and may default to a legacy code page on Windows.
            .env("PYTHONUNBUFFERED", "1")
            .env("PYTHONIOENCODING", "utf-8")
            .envs(&spec.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(e) => {
                return Exit::SpawnFailed(format!(
                    "cannot run {:?} in {}: {e}",
                    spec.command,
                    spec.working_dir.display()
                ))
            }
        };
        let (Some(mut stdin), Some(stdout), Some(stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            return Exit::Crashed("plugin pipes unavailable".into());
        };

        let (tx, mut rx) = mpsc::channel::<String>(OUTGOING_QUEUE);
        let writer = tokio::spawn(async move {
            while let Some(line) = rx.recv().await {
                let written = async {
                    stdin.write_all(line.as_bytes()).await?;
                    stdin.write_all(b"\n").await?;
                    stdin.flush().await
                };
                if written.await.is_err() {
                    break;
                }
            }
        });
        let name = spec.name.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if !line.trim().is_empty() {
                    info!(plugin = %name, "{}", line.trim_end());
                }
            }
        });
        let connection = Arc::new(Connection {
            plugin: spec.name.clone(),
            outgoing: tx,
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
        });
        let mut reader = tokio::spawn(read_loop(Arc::clone(&connection), stdout, Arc::clone(handler)));

        let handshake = tokio::select! {
            result = connection.call("initialize", init_params.clone(), startup_timeout) => result,
            status = child.wait() => {
                writer.abort();
                return Exit::Crashed(match status {
                    Ok(status) => format!("exited during startup ({status})"),
                    Err(e) => format!("exited during startup: {e}"),
                });
            }
        };
        let manifest = match handshake
            .map_err(|e| e.to_string())
            .and_then(|v| Manifest::parse(v, &spec.name))
        {
            Ok(manifest) => Arc::new(manifest),
            Err(e) => {
                let _ = child.kill().await;
                writer.abort();
                return Exit::Crashed(format!("initialize failed: {e}"));
            }
        };
        info!(
            plugin = %spec.name,
            version = %manifest.version,
            prefixes = ?manifest.search_prefixes,
            routes = manifest.routes.len(),
            events = ?manifest.events,
            "plugin started"
        );
        *self.manifest.write().unwrap() = Some(manifest);
        *self.connection.write().unwrap() = Some(Arc::clone(&connection));
        self.state.send_replace(PluginState::Running);

        let exit = tokio::select! {
            _ = shutdown.cancelled() => {
                connection.notify("shutdown", json!({}));
                if tokio::time::timeout(SHUTDOWN_GRACE, child.wait()).await.is_err() {
                    let _ = child.kill().await;
                }
                Exit::Shutdown
            }
            status = child.wait() => Exit::Crashed(match status {
                Ok(status) => format!("exited ({status})"),
                Err(e) => e.to_string(),
            }),
            _ = &mut reader => {
                // Usually the process is exiting; report its status when it does.
                match tokio::time::timeout(Duration::from_millis(500), child.wait()).await {
                    Ok(Ok(status)) => Exit::Crashed(format!("exited ({status})")),
                    _ => {
                        let _ = child.kill().await;
                        Exit::Crashed("closed its stdout".into())
                    }
                }
            }
        };
        *self.connection.write().unwrap() = None;
        connection.fail_pending();
        writer.abort();
        reader.abort();
        exit
    }
}

async fn read_loop(connection: Arc<Connection>, stdout: ChildStdout, handler: InboundHandler) {
    let mut lines = BufReader::new(stdout).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(message) = serde_json::from_str::<Value>(line) else {
            // Stray prints: keep them visible, they are usually debugging output.
            info!(plugin = %connection.plugin, "{line}");
            continue;
        };
        let method = message.get("method").and_then(Value::as_str).map(str::to_string);
        let id = message.get("id").filter(|v| !v.is_null()).cloned();
        match (method, id) {
            (None, Some(id)) => {
                let Some(tx) = id
                    .as_u64()
                    .and_then(|id| connection.pending.lock().unwrap().remove(&id))
                else {
                    debug!(plugin = %connection.plugin, %id, "response to an unknown request");
                    continue;
                };
                let result = match message.get("error").filter(|e| !e.is_null()) {
                    Some(error) => Err(PluginError::Remote {
                        plugin: connection.plugin.clone(),
                        code: error.get("code").and_then(Value::as_i64).unwrap_or(-32000),
                        message: error
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown error")
                            .to_string(),
                    }),
                    None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
                };
                let _ = tx.send(result);
            }
            (Some(method), id) => {
                let params = message.get("params").cloned().unwrap_or(Value::Null);
                if method == "log" {
                    log_from_plugin(&connection.plugin, &params);
                    continue;
                }
                let connection = Arc::clone(&connection);
                let handler = Arc::clone(&handler);
                tokio::spawn(async move {
                    let result = handler(connection.plugin.clone(), method, params).await;
                    let Some(id) = id else { return };
                    let reply = match result {
                        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
                        Err((code, message)) => {
                            json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
                        }
                    };
                    let _ = connection.outgoing.send(reply.to_string()).await;
                });
            }
            (None, None) => debug!(plugin = %connection.plugin, "message without id or method ignored"),
        }
    }
}

fn log_from_plugin(plugin: &str, params: &Value) {
    let message = params
        .get("message")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| params.to_string());
    match params.get("level").and_then(Value::as_str).unwrap_or("info") {
        "error" => error!(plugin, "{message}"),
        "warn" | "warning" => warn!(plugin, "{message}"),
        "debug" | "trace" => debug!(plugin, "{message}"),
        _ => info!(plugin, "{message}"),
    }
}
