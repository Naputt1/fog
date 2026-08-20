use crate::proxy::LogEntry;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::os::unix::io::RawFd;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

/// Status snapshot of a single service.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceStatus {
    /// Display name of the service.
    pub name: String,
    /// Whether the process is actively running.
    pub running: bool,
    /// Health check state: `pending`, `unknown`, `healthy`, or `unhealthy`.
    pub health: String,
}

/// Allocated port map for an instance (symbolic name -> host port).
pub type PortMap = std::collections::HashMap<String, u16>;

/// Native route as exposed via IPC (template form, resolved with PortMap+branch on the index side).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NativeRouteInfo {
    pub host: String,
    pub service: String,
    pub port: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path_prefix: Option<String>,
}

/// Status snapshot of the reverse proxy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyStatus {
    /// Whether the proxy server is currently running.
    pub running: bool,
    /// The port the proxy listens on.
    pub port: u16,
}

/// A single service handed over to a replacing instance.
///
/// The PTY master fd is delivered separately via SCM_RIGHTS.
pub struct HandoffItem {
    /// Display name of the service.
    pub name: String,
    /// Process group leader of the running service.
    pub pid: u32,
    /// A dup of the PTY master fd (now owned by the receiver).
    pub fd: RawFd,
}

/// A cloneable handle to a proxy's live request-log queue.
pub type ProxyLogHandle = Arc<Mutex<VecDeque<LogEntry>>>;

/// Shared state published by the TUI app and served over the IPC socket.
pub struct IpcState {
    /// Current status of all services.
    pub services: Arc<Mutex<Vec<ServiceStatus>>>,
    /// Current proxy status, if a proxy is configured.
    pub proxy: Arc<Mutex<Option<ProxyStatus>>>,
    /// Live handle to the running proxy's request-log queue, if a proxy is
    /// configured. Set once at startup by the App; stable across config
    /// hot-reloads (restart reuses the same queue).
    pub proxy_logs: Arc<Mutex<Option<ProxyLogHandle>>>,
    /// Name of the script currently running.
    pub script: String,
    /// Identity of the git project (worktree family) this instance belongs to.
    pub project: Option<String>,
    /// Branch this instance serves, if any.
    pub branch: Option<String>,
    /// Absolute path to the directory holding this instance's `fog.json`
    /// (its config dir). Set once before the IPC server spawns; stable for
    /// the lifetime of the process.
    pub config_dir: Option<String>,
    /// Allocated ports for this instance (symbolic -> host port).
    pub ports: Arc<Mutex<PortMap>>,
    /// Native routes (templates) for this instance.
    pub native_routes: Arc<Mutex<Vec<NativeRouteInfo>>>,
    /// Epoch milliseconds when this instance started.
    pub started_at: u64,
    /// Set to `true` when a kill request is received.
    pub kill_flag: Arc<AtomicBool>,
    /// Names of services whose `shutdown_cmd` should be skipped on exit
    /// (set by a reclaim/reuse kill request).
    pub reuse_skip: Arc<Mutex<Vec<String>>>,
    /// Services requested for handover by a replacing instance.
    pub handoff_req: Arc<Mutex<Option<Vec<String>>>>,
    /// Handoff results filled in by the App after a handover request.
    pub handoff_results: Arc<Mutex<Vec<HandoffItem>>>,
    /// Set once a reclaim connection has claimed the handoff right, so a
    /// second concurrent reclaim (or a plain kill) cannot steal it.
    pub handoff_claimed: Arc<AtomicBool>,
    /// Set by the App once handoffs have been prepared.
    pub handoff_prepared: Arc<AtomicBool>,
    /// Set by the IPC thread once handoffs have been sent to the requester.
    pub handoff_done: Arc<AtomicBool>,
    /// Per-service control request published by the IPC thread for the App to
    /// execute; cleared by the App once it has been taken.
    pub control_req: Arc<Mutex<Option<ServiceActionRequest>>>,
    /// The App's result for the last control request. The App is the only
    /// writer; the IPC thread takes it once `control_done` is set.
    pub control_result: Arc<Mutex<Option<ControlResponse>>>,
    /// Set by the App once the control request has been executed.
    pub control_done: Arc<AtomicBool>,
}

impl IpcState {
    /// Creates a new empty [`IpcState`] for the given script name.
    pub fn new(script: String, project: Option<String>, branch: Option<String>) -> Self {
        Self {
            services: Arc::new(Mutex::new(Vec::new())),
            proxy: Arc::new(Mutex::new(None)),
            proxy_logs: Arc::new(Mutex::new(None)),
            script,
            project,
            branch,
            config_dir: None,
            ports: Arc::new(Mutex::new(PortMap::new())),
            native_routes: Arc::new(Mutex::new(Vec::new())),
            started_at: crate::lock::now_ms(),
            kill_flag: Arc::new(AtomicBool::new(false)),
            reuse_skip: Arc::new(Mutex::new(Vec::new())),
            handoff_req: Arc::new(Mutex::new(None)),
            handoff_results: Arc::new(Mutex::new(Vec::new())),
            handoff_claimed: Arc::new(AtomicBool::new(false)),
            handoff_prepared: Arc::new(AtomicBool::new(false)),
            handoff_done: Arc::new(AtomicBool::new(false)),
            control_req: Arc::new(Mutex::new(None)),
            control_result: Arc::new(Mutex::new(None)),
            control_done: Arc::new(AtomicBool::new(false)),
        }
    }
}

/// The status response sent back to a `status` request.
#[derive(Debug, Serialize, Deserialize)]
pub struct StatusResponse {
    /// PID of the fog process.
    pub pid: u32,
    /// Name of the script being run.
    pub script: String,
    /// Service status snapshots.
    pub services: Vec<ServiceStatus>,
    /// Proxy status, if a proxy is configured.
    pub proxy: Option<ProxyStatus>,
    /// Git project identity of the instance, if it is inside a repository.
    #[serde(default)]
    pub project: Option<String>,
    /// Branch the instance serves, if any.
    #[serde(default)]
    pub branch: Option<String>,
    /// Absolute path to the directory holding this instance's `fog.json`
    /// (its config dir), if known.
    #[serde(default)]
    pub config_dir: Option<String>,
    /// Epoch milliseconds when the instance started (0 for older versions).
    #[serde(default)]
    pub started_at: u64,
    /// Allocated ports for this instance.
    #[serde(default)]
    pub ports: PortMap,
    /// Native routes (templates) for this instance.
    #[serde(default)]
    pub native_routes: Vec<NativeRouteInfo>,
}

fn default_log_tail() -> usize {
    200
}

/// A request received over the IPC socket.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum Request {
    Status,
    Kill {
        /// Names of services whose `shutdown_cmd` should be skipped, so a
        /// replacing instance can reuse them.
        #[serde(default)]
        reuse: Vec<String>,
    },
    Logs {
        /// Service whose captured log to stream; the special name `proxy`
        /// streams the proxy's live request log instead.
        service: String,
        /// Number of trailing lines to emit before any follow output.
        #[serde(default = "default_log_tail")]
        tail: usize,
        /// Keep streaming new output until the connection closes.
        #[serde(default)]
        follow: bool,
    },
    ServiceAction {
        /// Display name of the target service (or `"proxy"`).
        name: String,
        /// The action to perform.
        action: ServiceAction,
    },
}

/// The response sent back to a `kill` request.
#[derive(Debug, Serialize)]
pub(crate) struct KillResponse {
    pub(crate) ok: bool,
    /// Human-readable reason when `ok` is false.
    #[serde(default)]
    pub(crate) reason: String,
}

/// The action a `service_action` request asks the App to perform on a single
/// service.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceAction {
    /// Start the service (fails if it is already running).
    Start,
    /// Stop the service without respawning it.
    Stop,
    /// Stop the service and spawn it again.
    Restart,
}

/// The response sent back to a `service_action` request.
#[derive(Debug, Serialize, Deserialize)]
pub struct ControlResponse {
    /// Whether the action was executed.
    pub ok: bool,
    /// Human-readable reason when `ok` is false (or a timeout occurred).
    #[serde(default)]
    pub reason: String,
}

/// A per-service control request published by the IPC thread for the App to
/// execute. The App is the only consumer of these fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceActionRequest {
    /// Display name of the target service (or `"proxy"`).
    pub name: String,
    /// The action to perform.
    pub action: ServiceAction,
}
