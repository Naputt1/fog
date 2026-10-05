//! Health-check probing and the adaptive per-service health loop.

use crate::config::HealthCheckConfig;
use std::net::ToSocketAddrs;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::{Duration, Instant};

use super::{
    DEFAULT_HEALTH_INTERVAL, DEFAULT_HEALTH_RETRIES, DEFAULT_START_INTERVAL, MIN_HEALTH_INTERVAL,
};

/// Fires a wake-up ping to the app loop whenever any service's health status
/// changes, so dependents gated by `depends_on` start the moment a dependency
/// becomes ready instead of waiting for the next poll tick.
pub struct HealthSignal {
    subscribers: Mutex<Vec<std::sync::mpsc::Sender<()>>>,
}

impl HealthSignal {
    pub(super) fn new() -> Self {
        Self {
            subscribers: Mutex::new(Vec::new()),
        }
    }

    /// Subscribes to health-change pings. The loop wakes when it receives.
    pub fn subscribe(&self) -> std::sync::mpsc::Receiver<()> {
        let (tx, rx) = std::sync::mpsc::channel();
        self.subscribers
            .lock()
            .expect("health signal mutex poisoned")
            .push(tx);
        rx
    }

    /// Pings every subscriber, pruning any whose receiver has been dropped.
    pub fn notify(&self) {
        let mut subs = self
            .subscribers
            .lock()
            .expect("health signal mutex poisoned");
        subs.retain(|tx| tx.send(()).is_ok());
    }
}

/// Process-wide health-change signal shared by every health-check thread and the
/// app loop. A single orchestrator instance runs at a time, so a shared source
/// is sufficient and avoids threading an `Arc` through every terminal.
pub fn health_signal() -> &'static Arc<HealthSignal> {
    static SIGNAL: std::sync::OnceLock<Arc<HealthSignal>> = std::sync::OnceLock::new();
    SIGNAL.get_or_init(|| Arc::new(HealthSignal::new()))
}

pub(super) fn clamp_interval(ms: u64) -> Duration {
    Duration::from_millis(ms).max(MIN_HEALTH_INTERVAL)
}

/// Health check status for a terminal.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum HealthStatus {
    /// Service is waiting for dependencies to start.
    Pending,
    Unknown,
    /// Health checks have not passed yet, but the service is still within its
    /// startup grace window (`start_period_ms`) or below `retries`.
    Starting,
    Healthy,
    Unhealthy,
}

/// Probes a single health-check target. Both `tcp` and `http` kinds use a TCP
/// connect, so they share this implementation. The `docker` kind checks the
/// actual container from the configured compose file.
fn check_target(config: &HealthCheckConfig, branch: Option<&str>) -> bool {
    match config.kind {
        crate::config::HealthCheckKind::Docker => check_docker_target(config, branch),
        _ => {
            let timeout = config.timeout_ms.unwrap_or(2000);
            let addr = config
                .target
                .trim_start_matches("tcp://")
                .trim_start_matches("http://")
                .trim_start_matches("https://");
            addr.to_socket_addrs()
                .ok()
                .map(|addrs| {
                    addrs.into_iter().any(|sa| {
                        std::net::TcpStream::connect_timeout(
                            &sa,
                            std::time::Duration::from_millis(timeout),
                        )
                        .is_ok()
                    })
                })
                .unwrap_or(false)
        }
    }
}

/// Verifies a compose service is running (and, when the compose file defines a
/// healthcheck for it, reports `healthy`) by inspecting `docker compose ps`.
///
/// The compose file is resolved relative to the service's working directory at
/// build time, so `config.compose_file` is already absolute here.
///
/// `branch` (the worktree branch, exported to services as `FOG_BRANCH` (slug)
/// and `FOG_BRANCH_RAW` (raw)) is forwarded to the subprocess so
/// branch-suffixed compose project names (e.g. `redfox-${FOG_BRANCH:-main}`)
/// resolve to the running project instead of the `main` default.
pub(super) fn check_docker_target(config: &HealthCheckConfig, branch: Option<&str>) -> bool {
    let timeout = config.timeout_ms.unwrap_or(2000);
    let compose_file = config
        .compose_file
        .clone()
        .unwrap_or_else(|| "docker-compose.yml".to_string());

    let mut cmd = std::process::Command::new("docker");
    cmd.args(["compose", "-f", &compose_file, "ps", "--format", "json"])
        .arg(&config.target)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    if let Some(branch) = branch {
        if let Ok(slug) = crate::ports::branch_slug(branch) {
            cmd.env("FOG_BRANCH", slug.clone());
            cmd.env("FOG_BRANCH_SLUG", slug);
        }
        cmd.env("FOG_BRANCH_RAW", branch);
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(_) => return false,
    };

    let deadline = Instant::now() + Duration::from_millis(timeout);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    break None;
                }
                thread::sleep(Duration::from_millis(50));
            }
            Err(_) => break None,
        }
    };

    let Some(status) = status else {
        return false;
    };
    if !status.success() {
        return false;
    }

    let Ok(out) = child.wait_with_output() else {
        return false;
    };
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    docker_ps_is_healthy(&stdout)
}

/// Parses the JSON output of `docker compose ps --format json <service>`.
/// Passes when the service is running; when a `Health` field is present and
/// non-empty it must equal `healthy`.
pub(super) fn docker_ps_is_healthy(stdout: &str) -> bool {
    let value: serde_json::Value = match serde_json::from_str(stdout) {
        Ok(v) => v,
        Err(_) => return false,
    };

    let entries: Vec<&serde_json::Value> = match &value {
        serde_json::Value::Array(items) => items.iter().collect(),
        serde_json::Value::Object(_) => vec![&value],
        _ => return false,
    };

    entries.into_iter().any(|entry| {
        let running = entry
            .get("State")
            .and_then(|s| s.as_str())
            .is_some_and(|s| s == "running");
        if !running {
            return false;
        }
        match entry.get("Health").and_then(|h| h.as_str()) {
            Some(h) if !h.is_empty() => h == "healthy",
            _ => true,
        }
    })
}

/// Evaluates health checks, returning `true` only when ALL of them pass.
///
/// Checks run concurrently so the result is bounded by the slowest check
/// rather than the sum — important for the synchronous startup probe of reused
/// services, which would otherwise add `timeout_ms` per check to startup.
///
/// `branch` is forwarded to `docker`-kind checks so they resolve the
/// branch-suffixed compose project (see [`check_docker_target`]).
pub fn health_checks_pass(configs: &[HealthCheckConfig], branch: Option<&str>) -> bool {
    thread::scope(|s| {
        configs
            .iter()
            .map(|c| {
                let c = c.clone();
                s.spawn(move || check_target(&c, branch))
            })
            .collect::<Vec<_>>()
            .into_iter()
            .all(|h| h.join().unwrap_or(false))
    })
}

/// Runs the adaptive health-check loop for `configs` on a background thread,
/// updating `status` until `stop` is set. Shared by a terminal's own health
/// checks and each of its declared endpoints.
pub(super) fn spawn_health_loop(
    configs: Vec<HealthCheckConfig>,
    branch: Option<String>,
    status: Arc<Mutex<HealthStatus>>,
    stop: Arc<AtomicBool>,
) {
    if configs.is_empty() {
        return;
    }
    thread::spawn(move || {
        let start_interval = clamp_interval(
            configs
                .iter()
                .filter_map(|c| c.start_interval_ms)
                .min()
                .unwrap_or(DEFAULT_START_INTERVAL.as_millis() as u64),
        );
        let interval = clamp_interval(
            configs
                .iter()
                .filter_map(|c| c.interval_ms)
                .min()
                .unwrap_or(DEFAULT_HEALTH_INTERVAL.as_millis() as u64),
        );
        let start_period = Duration::from_millis(
            configs
                .iter()
                .filter_map(|c| c.start_period_ms)
                .max()
                .unwrap_or(0),
        );
        let retries = configs
            .iter()
            .map(|c| c.retries.unwrap_or(DEFAULT_HEALTH_RETRIES))
            .max()
            .unwrap_or(DEFAULT_HEALTH_RETRIES)
            .max(1);

        let started = Instant::now();
        let mut failures: u32 = 0;
        let mut ever_healthy = false;
        let mut last = HealthStatus::Unknown;

        loop {
            if stop.load(Ordering::SeqCst) {
                return;
            }
            let pass = health_checks_pass(&configs, branch.as_deref());
            let next = if pass {
                failures = 0;
                ever_healthy = true;
                HealthStatus::Healthy
            } else {
                failures = failures.saturating_add(1);
                if !ever_healthy && started.elapsed() < start_period {
                    HealthStatus::Starting
                } else if failures < retries {
                    if last == HealthStatus::Healthy {
                        HealthStatus::Healthy
                    } else {
                        HealthStatus::Starting
                    }
                } else {
                    HealthStatus::Unhealthy
                }
            };

            if next != last {
                *status.lock().expect("health status mutex poisoned") = next;
                last = next;
                health_signal().notify();
            }

            let sleep_for = if ever_healthy {
                interval
            } else {
                start_interval
            };
            let deadline = Instant::now() + sleep_for;
            while Instant::now() < deadline {
                if stop.load(Ordering::SeqCst) {
                    return;
                }
                let remaining = deadline.saturating_duration_since(Instant::now());
                thread::sleep(remaining.min(Duration::from_millis(50)));
            }
        }
    });
}
