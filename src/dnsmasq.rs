use crate::config::DnsmasqConfig;
use std::fs;
use std::io::Write;
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

/// Applies the configured dnsmasq wildcard-DNS setup on startup.
///
/// This is best-effort and never fails the run: it returns human-readable
/// messages (info or warnings) for the caller to print. Missing dnsmasq, a
/// missing Homebrew prefix, or an unsupported platform produce a warning and
/// leave the run untouched.
///
/// In `non_interactive` mode (detached daemons, no TTY) sudo is invoked with
/// `-n` so it cannot hang on a password prompt; if a privileged step cannot
/// run, a warning tells the user to run `fog <script>` interactively once.
pub fn ensure(cfg: &DnsmasqConfig, non_interactive: bool) -> Vec<String> {
    let mut messages = Vec::new();

    if !command_exists("dnsmasq") {
        messages.push(format!(
            "⚠ dnsmasq is not installed; skipping wildcard DNS setup for {} \
             (install with `brew install dnsmasq` and re-run)",
            cfg.domains.join(", ")
        ));
        return messages;
    }

    match apply(cfg, non_interactive, &mut messages) {
        Ok(()) => {}
        Err(e) => messages.push(format!("⚠ dnsmasq setup skipped: {e}")),
    }

    messages
}

/// Platform-specific application of the wildcard routes.
fn apply(
    cfg: &DnsmasqConfig,
    non_interactive: bool,
    messages: &mut Vec<String>,
) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        apply_macos(cfg, non_interactive, messages)
    }
    #[cfg(target_os = "linux")]
    {
        apply_linux(cfg, non_interactive, messages)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        Err(format!(
            "wildcard DNS setup is only supported on macOS and Linux (running on {})",
            std::env::consts::OS
        ))
    }
}

/// macOS (Homebrew) dnsmasq: config lives under `/opt/homebrew` or
/// `/usr/local`, and per-zone resolvers live in `/etc/resolver`.
#[cfg(target_os = "macos")]
fn apply_macos(
    cfg: &DnsmasqConfig,
    non_interactive: bool,
    messages: &mut Vec<String>,
) -> Result<(), String> {
    let prefix = detect_homebrew_prefix().ok_or_else(|| {
        "could not find a Homebrew dnsmasq.conf under /opt/homebrew or /usr/local".to_string()
    })?;
    apply_macos_at(
        cfg,
        &prefix,
        Path::new("/etc/resolver"),
        non_interactive,
        true,
        messages,
    )
}

/// Shared macOS implementation, parameterized for testability: `prefix` is the
/// Homebrew root, `resolver_dir` is where per-zone resolver files are written,
/// and `restart` controls whether dnsmasq is restarted after changes.
#[cfg(target_os = "macos")]
fn apply_macos_at(
    cfg: &DnsmasqConfig,
    prefix: &Path,
    resolver_dir: &Path,
    non_interactive: bool,
    restart: bool,
    messages: &mut Vec<String>,
) -> Result<(), String> {
    let dnsmasq_d = prefix.join("etc/dnsmasq.d");
    let dnsmasq_conf = prefix.join("etc/dnsmasq.conf");
    fs::create_dir_all(&dnsmasq_d)
        .map_err(|e| format!("could not create {}: {}", dnsmasq_d.display(), e))?;

    let mut changed = false;

    // Enable `conf-dir` so the per-domain files below are read.
    if !read_optional(&dnsmasq_conf)?.lines().any(|l| {
        l.trim().starts_with("conf-dir=") && l.contains(&dnsmasq_d.to_string_lossy().into_owned())
    }) {
        append_line(&dnsmasq_conf, &format!("conf-dir={}", dnsmasq_d.display()))?;
        changed = true;
    }

    // Write one idempotent file per domain.
    for domain in &cfg.domains {
        let line = format!("address=/.{}/{}", domain, cfg.address);
        let file = dnsmasq_d.join(format!("fog-{domain}.conf"));
        if !read_optional(&file)?.lines().any(|l| l.trim() == line) {
            write_file(&file, &format!("{line}\n"))?;
            changed = true;
            messages.push(format!(
                "  + wrote *.{domain} route to {file}",
                domain = domain,
                file = file.display(),
            ));
        }

        // macOS resolver file for the zone: a plain `nameserver` line only.
        // (macOS renders but ignores a `port` directive in /etc/resolver files,
        // so dnsmasq must sit on the standard :53 — hence the root daemon.)
        let resolver = resolver_dir.join(domain);
        let resolver_content = format!("nameserver {}\n", cfg.address);
        if read_optional(&resolver)? != resolver_content {
            match sudo_write(&resolver, &resolver_content, non_interactive) {
                Ok(()) => {
                    changed = true;
                    messages.push(format!(
                        "  + created {} for zone {}.",
                        resolver.display(),
                        domain
                    ));
                }
                Err(e) => messages.push(format!(
                    "⚠ could not write {} ({}). DNS for *.{domain} won't resolve until this is \
                     done; run `fog <script>` interactively once.",
                    resolver.display(),
                    e,
                )),
            }
        }
    }

    // Pin dnsmasq to the intended port on loopback only. The daemon runs as
    // root (macOS LaunchDaemon), so it can bind the privileged :53 default.
    let port_conf = dnsmasq_d.join("fog-port.conf");
    let port_content = format!(
        "port={}\nlisten-address={}\nbind-interfaces\n",
        cfg.port, cfg.address
    );
    if read_optional(&port_conf)? != port_content {
        write_file(&port_conf, &port_content)?;
        changed = true;
        messages.push(format!(
            "  + wrote dnsmasq port binding ({address}:{port}) to {file}",
            address = cfg.address,
            port = cfg.port,
            file = port_conf.display(),
        ));
    }

    if restart {
        ensure_running(changed, &cfg.address, cfg.port, non_interactive, messages);
    }

    Ok(())
}

/// Linux dnsmasq: config drops into `/etc/dnsmasq.d`, restart via systemctl.
#[cfg(target_os = "linux")]
fn apply_linux(
    cfg: &DnsmasqConfig,
    non_interactive: bool,
    messages: &mut Vec<String>,
) -> Result<(), String> {
    let dnsmasq_d = PathBuf::from("/etc/dnsmasq.d");

    let mut changed = false;
    for domain in &cfg.domains {
        let line = format!("address=/.{}/{}", domain, cfg.address);
        let file = dnsmasq_d.join(format!("fog-{domain}.conf"));
        match sudo_read_contains(&file, &line, non_interactive) {
            Ok(true) => {}
            Ok(false) => {
                let content = format!("{line}\n");
                sudo_write(&file, &content, non_interactive).map_err(|e| {
                    format!(
                        "could not write {} ({}); run interactively once",
                        file.display(),
                        e
                    )
                })?;
                changed = true;
                messages.push(format!(
                    "  + wrote *.{domain} route to {file}",
                    domain = domain,
                    file = file.display(),
                ));
            }
            Err(e) => messages.push(format!(
                "⚠ could not check {} ({}); skipping. Run `fog <script>` interactively once.",
                file.display(),
                e
            )),
        }
    }

    // Pin dnsmasq to `address:port` so it never collides with a system
    // resolver and so the resolver config below matches.
    let port_conf = dnsmasq_d.join("fog-port.conf");
    let port_content = format!("port={}\nlisten-address={}\n", cfg.port, cfg.address);
    match sudo_read_contains(&port_conf, &port_content, non_interactive) {
        Ok(true) => {}
        Ok(false) => {
            sudo_write(&port_conf, &port_content, non_interactive).map_err(|e| {
                format!(
                    "could not write {} ({}); run interactively once",
                    port_conf.display(),
                    e
                )
            })?;
            changed = true;
            messages.push(format!(
                "  + wrote dnsmasq port binding ({address}:{port}) to {file}",
                address = cfg.address,
                port = cfg.port,
                file = port_conf.display(),
            ));
        }
        Err(e) => messages.push(format!(
            "⚠ could not check {} ({}); skipping.",
            port_conf.display(),
            e
        )),
    }

    ensure_running(changed, &cfg.address, cfg.port, non_interactive, messages);

    Ok(())
}

#[cfg(target_os = "macos")]
fn detect_homebrew_prefix() -> Option<PathBuf> {
    for candidate in ["/opt/homebrew", "/usr/local"] {
        let conf = Path::new(candidate).join("etc/dnsmasq.conf");
        if conf.exists() {
            return Some(PathBuf::from(candidate));
        }
    }
    None
}

fn command_exists(name: &str) -> bool {
    Command::new("sh")
        .args(["-c", &format!("command -v {name} >/dev/null 2>&1")])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Reads a file, treating a missing file as empty content. Used by the
/// idempotency checks so a first run writes the config instead of erroring.
fn read_optional(path: &Path) -> Result<String, String> {
    match fs::read_to_string(path) {
        Ok(s) => Ok(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(format!("could not read {}: {}", path.display(), e)),
    }
}

fn write_file(path: &Path, content: &str) -> Result<(), String> {
    fs::write(path, content).map_err(|e| format!("could not write {}: {}", path.display(), e))
}

fn append_line(path: &Path, line: &str) -> Result<(), String> {
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| format!("could not open {} for append: {}", path.display(), e))?;
    writeln!(f, "{line}").map_err(|e| format!("could not write {}: {}", path.display(), e))
}

/// Writes `content` to `path`, escalating to `sudo` when the path is not
/// writable. In non-interactive mode sudo runs with `-n` so it cannot block on
/// a password prompt.
fn sudo_write(path: &Path, content: &str, non_interactive: bool) -> Result<(), String> {
    if let Some(parent) = path.parent()
        && !parent.exists()
    {
        run_sudo(&["mkdir", "-p", &parent.to_string_lossy()], non_interactive)?;
    }
    if fs::write(path, content).is_ok() {
        return Ok(());
    }
    // Not writable: stage a temp file in the OS temp dir and install it via
    // sudo (we cannot pipe stdin through `sudo cat`).
    let tmp = std::env::temp_dir().join(format!(
        "fog-dnsmasq-stage-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    fs::write(&tmp, content).map_err(|e| format!("could not stage {}: {}", tmp.display(), e))?;
    let res = run_sudo(
        &[
            "sh",
            "-c",
            &format!(
                "install -m 644 '{}' '{}' && rm -f '{}'",
                tmp.display(),
                path.display(),
                tmp.display()
            ),
        ],
        non_interactive,
    );
    if res.is_ok() {
        Ok(())
    } else {
        let _ = fs::remove_file(&tmp);
        Err(format!("sudo write to {} failed", path.display()))
    }
}

#[cfg(target_os = "linux")]
fn sudo_read_contains(path: &Path, needle: &str, non_interactive: bool) -> Result<bool, String> {
    // A missing file means "route not configured yet" — the caller will write
    // it, rather than this being treated as an error.
    let exists = match run_sudo_output(&["test", "-e", &path.to_string_lossy()], non_interactive) {
        Ok(_) => true,
        Err(_) => false,
    };
    if exists {
        let out = run_sudo_output(&["cat", &path.to_string_lossy()], non_interactive)?;
        return Ok(out.lines().any(|l| l.trim() == needle));
    }
    Ok(false)
}

/// Returns `true` if the dnsmasq daemon is currently serving on `address:port`.
/// A TCP connect probes the actual listener rather than relying on process
/// tables, which can report a stopped daemon.
fn is_running(address: &str, port: u16) -> bool {
    let addr: Option<SocketAddr> = format!("{address}:{port}")
        .to_socket_addrs()
        .ok()
        .and_then(|mut a| a.next());
    let Some(addr) = addr else {
        return false;
    };
    TcpStream::connect_timeout(&addr, Duration::from_millis(300)).is_ok()
}

/// Decides what to do with the dnsmasq service given whether the config
/// changed and whether the daemon is currently up:
///
/// * `start` — the daemon is down; bring it up.
/// * `restart` — the daemon is up but the config changed; reload it.
/// * `None` — the daemon is up and the config is unchanged; do nothing.
///
/// Split out so the decision logic is pure and testable.
fn next_service_action(changed: bool, running: bool) -> Option<&'static str> {
    match (running, changed) {
        (false, _) => Some("start"),
        (true, true) => Some("restart"),
        (true, false) => None,
    }
}

/// Ensures the dnsmasq daemon is running (starting it if it is down), and
/// restarts it when the config changed so new routes take effect. Reports
/// outcomes through `messages`; never fails the run.
///
/// This replaces the old "restart only when config changed" behavior: a
/// stopped-but-correctly-configured dnsmasq now gets started automatically.
fn ensure_running(
    changed: bool,
    address: &str,
    port: u16,
    non_interactive: bool,
    messages: &mut Vec<String>,
) {
    let running = is_running(address, port);
    let action = match next_service_action(changed, running) {
        Some(a) => a,
        None => return,
    };
    service_dnsmasq(action, non_interactive, messages);
    // After starting, give the daemon a moment to bind before confirming.
    if action == "start" {
        for _ in 0..10 {
            if is_running(address, port) {
                break;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        if is_running(address, port) {
            messages.push(format!(
                "  + dnsmasq is up on {address}:{port} and resolving wildcard domains"
            ));
        } else {
            messages.push(
                "⚠ dnsmasq did not come up after starting; check `brew services list dnsmasq` \
                 or system logs."
                    .to_string(),
            );
        }
    }
}

/// Starts or restarts the dnsmasq service through the platform service manager.
///
/// On macOS this uses `sudo brew services <action> dnsmasq`, which installs a
/// **root LaunchDaemon** in `/Library/LaunchDaemons/` — required so dnsmasq can
/// bind the privileged DNS port :53 (a user LaunchAgent cannot). Before
/// starting, any stale user-level `homebrew.mxcl.dnsmasq` agent is booted out
/// so it cannot compete for the port. On Linux it uses
/// `sudo systemctl <action> dnsmasq`, honoring the headless `sudo -n` guard.
fn service_dnsmasq(action: &str, non_interactive: bool, messages: &mut Vec<String>) {
    #[cfg(target_os = "linux")]
    {
        let mut c = Command::new("sudo");
        if non_interactive {
            // Never block on a password prompt in headless runs.
            c.arg("-n");
        }
        c.args(["systemctl", action, "dnsmasq"]);
        match c.status() {
            Ok(s) if s.success() => messages.push(format!(
                "  + dnsmasq {action}ed (sudo systemctl {action} dnsmasq)"
            )),
            Ok(_) | Err(_) => {
                messages.push(format!(
                    "⚠ could not {action} dnsmasq (sudo systemctl {action} dnsmasq). Existing \
                     routes may not be active yet."
                ));
            }
        }
        return;
    }

    #[cfg(target_os = "macos")]
    {
        // A stale user-level brew Agent (from the pre-root era) would fail to
        // bind :53 and could crash-loop or shadow the daemon. Remove it first.
        let _ = bootout_user_agent(non_interactive);

        // Running `brew services` as root installs a LaunchDaemon in
        // /Library/LaunchDaemons so dnsmasq runs as root and can bind :53.
        let mut c = Command::new("sudo");
        if non_interactive {
            // Never block on a password prompt in headless runs.
            c.arg("-n");
        }
        c.args(["brew", "services", action, "dnsmasq"]);
        match c.status() {
            Ok(s) if s.success() => messages.push(format!(
                "  + dnsmasq {action}ed (sudo brew services {action} dnsmasq)"
            )),
            Ok(_) | Err(_) => {
                messages.push(format!(
                    "⚠ could not {action} dnsmasq (sudo brew services {action} dnsmasq). Run \
                     `fog <script>` interactively once and approve the sudo prompt. Existing \
                     routes may not be active yet."
                ));
            }
        }
    }
}

/// Boots out any user-level `homebrew.mxcl.dnsmasq` LaunchAgent so it cannot
/// compete with (or shadow) the root LaunchDaemon draining :53.
#[cfg(target_os = "macos")]
fn bootout_user_agent(non_interactive: bool) -> Result<(), String> {
    let domain = format!("gui/{}", unsafe { libc::getuid() });
    // Only try to unload if it is actually registered; otherwise bootout fails
    // with a confusing error.
    let loaded = Command::new("launchctl")
        .args(["print", &domain, "homebrew.mxcl.dnsmasq"])
        .output()
        .ok()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !loaded {
        return Ok(());
    }
    let mut c = Command::new("sudo");
    if non_interactive {
        c.arg("-n");
    }
    c.args(["launchctl", "bootout", &domain, "homebrew.mxcl.dnsmasq"]);
    match c.status() {
        Ok(s) if s.success() => Ok(()),
        Ok(_) | Err(_) => Err("launchctl bootout homebrew.mxcl.dnsmasq failed".to_string()),
    }
}

fn run_sudo(args: &[&str], non_interactive: bool) -> Result<(), String> {
    let mut cmd = Command::new("sudo");
    if non_interactive {
        cmd.arg("-n");
    }
    cmd.args(args);
    let status = cmd.status().map_err(|e| format!("sudo: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("sudo exited with {}", status))
    }
}

#[cfg(target_os = "linux")]
fn run_sudo_output(args: &[&str], non_interactive: bool) -> Result<String, String> {
    let mut cmd = Command::new("sudo");
    if non_interactive {
        cmd.arg("-n");
    }
    cmd.args(args);
    let out = cmd.output().map_err(|e| format!("sudo: {e}"))?;
    if !out.status.success() {
        return Err(format!("sudo exited with {}", out.status));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(domains: &[&str]) -> DnsmasqConfig {
        DnsmasqConfig {
            domains: domains.iter().map(|s| s.to_string()).collect(),
            address: "127.0.0.1".to_string(),
            port: 53,
        }
    }

    #[test]
    fn test_missing_dnsmasq_warns() {
        // When dnsmasq is missing, ensure() warns and does not panic. When it
        // IS installed, ensure() still returns without panicking. Either way it
        // must produce messages or a clean Ok — never a crash.
        let _msgs = ensure(&cfg(&["red-fox"]), true);
        // No assertion on content: depends on whether dnsmasq exists on the
        // machine running the test.
    }

    #[test]
    fn test_next_action_starts_when_down() {
        assert_eq!(next_service_action(false, false), Some("start"));
        assert_eq!(next_service_action(true, false), Some("start"));
    }

    #[test]
    fn test_next_action_restarts_when_changed_and_up() {
        assert_eq!(next_service_action(true, true), Some("restart"));
    }

    #[test]
    fn test_next_action_noop_when_up_and_unchanged() {
        assert_eq!(next_service_action(false, true), None);
    }

    #[test]
    fn test_is_running_returns_bool_without_panic() {
        // No assertion on the value: it depends on whether a resolver is bound
        // to the configured address:port on the test machine. It must simply
        // return a bool, never panic.
        let _running = is_running("127.0.0.1", 53);
    }

    #[test]
    fn test_address_line_format() {
        let c = cfg(&["red-fox"]);
        let line = format!("address=/.{}/{}", c.domains[0], c.address);
        assert_eq!(line, "address=/.red-fox/127.0.0.1");
    }

    #[test]
    fn test_read_optional_missing_is_empty() {
        let missing = std::env::temp_dir().join(format!(
            "fog-dnsmasq-missing-{}-{}.conf",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        assert_eq!(read_optional(&missing).unwrap(), "");
    }

    #[test]
    fn test_read_optional_present_returns_content() {
        let file =
            std::env::temp_dir().join(format!("fog-dnsmasq-present-{}.conf", std::process::id()));
        fs::write(&file, "address=/.red-fox/127.0.0.1\n").unwrap();
        assert_eq!(
            read_optional(&file).unwrap(),
            "address=/.red-fox/127.0.0.1\n"
        );
        let _ = fs::remove_file(&file);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn test_apply_macos_first_run_writes_config() {
        // A temp "Homebrew prefix" that mirrors the real layout, exercising the
        // first-run path that previously errored on the missing conf file.
        let base = std::env::temp_dir().join(format!(
            "fog-dnsmasq-prefix-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let d = base.join("etc/dnsmasq.d");
        let conf = base.join("etc/dnsmasq.conf");
        fs::create_dir_all(&d).unwrap();
        // Homebrew ships an active conf-dir line.
        fs::write(&conf, format!("conf-dir={}\n", d.display())).unwrap();

        let resolver_dir = base.join("resolver");
        fs::create_dir_all(&resolver_dir).unwrap();

        let mut messages = Vec::new();
        let res = apply_macos_at(
            &cfg(&["red-fox"]),
            &base,
            &resolver_dir,
            true,
            false,
            &mut messages,
        );
        assert!(res.is_ok(), "first run must not error: {res:?}");

        let written = fs::read_to_string(d.join("fog-red-fox.conf")).unwrap();
        assert_eq!(written, "address=/.red-fox/127.0.0.1\n");
        assert_eq!(
            fs::read_to_string(resolver_dir.join("red-fox")).unwrap(),
            "nameserver 127.0.0.1\n"
        );
        assert_eq!(
            fs::read_to_string(d.join("fog-port.conf")).unwrap(),
            "port=53\nlisten-address=127.0.0.1\nbind-interfaces\n"
        );

        // Idempotent: a second run must not error and must not duplicate.
        let mut messages2 = Vec::new();
        let res2 = apply_macos_at(
            &cfg(&["red-fox"]),
            &base,
            &resolver_dir,
            true,
            false,
            &mut messages2,
        );
        assert!(res2.is_ok());
        assert_eq!(
            fs::read_to_string(d.join("fog-red-fox.conf")).unwrap(),
            "address=/.red-fox/127.0.0.1\n",
            "second run must not rewrite the route"
        );

        let _ = fs::remove_dir_all(&base);
    }
}
