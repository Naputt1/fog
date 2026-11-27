use crate::config::DnsmasqConfig;
use crate::error::{FogError, FogResult};
#[cfg(target_os = "macos")]
use crate::process::run_with_timeout;
use std::fs;
#[cfg(target_os = "macos")]
use std::io::Write;
use std::net::{Ipv4Addr, SocketAddr, TcpStream, ToSocketAddrs, UdpSocket};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Whether an interactive `sudo -v` has already cached credentials for this
/// process. All privileged steps share one prompt instead of one per command.
static SUDO_PRIMED: AtomicBool = AtomicBool::new(false);

/// Set once an interactive `sudo -v` fails, so a declined prompt is not asked
/// again for every remaining step in the same startup.
static SUDO_DENIED: AtomicBool = AtomicBool::new(false);

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
fn apply(cfg: &DnsmasqConfig, non_interactive: bool, messages: &mut Vec<String>) -> FogResult<()> {
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
        Err(FogError::new(format!(
            "wildcard DNS setup is only supported on macOS and Linux (running on {})",
            std::env::consts::OS
        )))
    }
}

/// Validates a configured domain before it is interpolated into a filesystem
/// path or a dnsmasq config line. Accepts a lowercase dotted hostname whose
/// labels are `[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?`, which rejects path
/// separators, quotes, whitespace, shell metacharacters, uppercase, and `..`.
fn validate_domain(domain: &str) -> FogResult<()> {
    let bad = || {
        FogError::new(format!(
            "invalid domain {domain:?}: must be a lowercase hostname"
        ))
    };
    if domain.is_empty() || domain.len() > 253 || domain.starts_with('.') || domain.ends_with('.') {
        return Err(bad());
    }
    let is_alnum = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    for label in domain.split('.') {
        if label.is_empty() || label.len() > 63 {
            return Err(bad());
        }
        let bytes = label.as_bytes();
        if !is_alnum(bytes[0]) || !is_alnum(bytes[bytes.len() - 1]) {
            return Err(bad());
        }
        if !bytes.iter().all(|b| is_alnum(*b) || *b == b'-') {
            return Err(bad());
        }
    }
    Ok(())
}

/// macOS (Homebrew) dnsmasq: config lives under `/opt/homebrew` or
/// `/usr/local`, and per-zone resolvers live in `/etc/resolver`.
#[cfg(target_os = "macos")]
fn apply_macos(
    cfg: &DnsmasqConfig,
    non_interactive: bool,
    messages: &mut Vec<String>,
) -> FogResult<()> {
    let prefix = detect_homebrew_prefix().ok_or_else(|| {
        FogError::new("could not find a Homebrew dnsmasq.conf under /opt/homebrew or /usr/local")
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
) -> FogResult<()> {
    let dnsmasq_d = prefix.join("etc/dnsmasq.d");
    let dnsmasq_conf = prefix.join("etc/dnsmasq.conf");
    fs::create_dir_all(&dnsmasq_d)
        .map_err(|e| FogError::new(format!("could not create {}: {}", dnsmasq_d.display(), e)))?;

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
        validate_domain(domain)?;
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
        ensure_running(
            changed,
            &cfg.address,
            cfg.port,
            &cfg.domains,
            non_interactive,
            messages,
        );
    }

    Ok(())
}

/// Linux dnsmasq: config drops into `/etc/dnsmasq.d`, restart via systemctl.
#[cfg(target_os = "linux")]
fn apply_linux(
    cfg: &DnsmasqConfig,
    non_interactive: bool,
    messages: &mut Vec<String>,
) -> FogResult<()> {
    let dnsmasq_d = PathBuf::from("/etc/dnsmasq.d");

    let mut changed = false;
    for domain in &cfg.domains {
        validate_domain(domain)?;
        let line = format!("address=/.{}/{}", domain, cfg.address);
        let file = dnsmasq_d.join(format!("fog-{domain}.conf"));
        match sudo_read_contains(&file, &line, non_interactive) {
            Ok(true) => {}
            Ok(false) => {
                let content = format!("{line}\n");
                sudo_write(&file, &content, non_interactive).map_err(|e| {
                    FogError::new(format!(
                        "could not write {} ({}); run interactively once",
                        file.display(),
                        e
                    ))
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
                FogError::new(format!(
                    "could not write {} ({}); run interactively once",
                    port_conf.display(),
                    e
                ))
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

    ensure_running(
        changed,
        &cfg.address,
        cfg.port,
        &cfg.domains,
        non_interactive,
        messages,
    );

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
    crate::process::command_exists(name)
}

/// Reads a file, treating a missing file as empty content. Used by the
/// idempotency checks so a first run writes the config instead of erroring.
#[cfg(target_os = "macos")]
fn read_optional(path: &Path) -> FogResult<String> {
    match fs::read_to_string(path) {
        Ok(s) => Ok(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(FogError::new(format!(
            "could not read {}: {}",
            path.display(),
            e
        ))),
    }
}

#[cfg(target_os = "macos")]
fn write_file(path: &Path, content: &str) -> FogResult<()> {
    fs::write(path, content)
        .map_err(|e| FogError::new(format!("could not write {}: {}", path.display(), e)))
}

#[cfg(target_os = "macos")]
fn append_line(path: &Path, line: &str) -> FogResult<()> {
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| {
            FogError::new(format!(
                "could not open {} for append: {}",
                path.display(),
                e
            ))
        })?;
    writeln!(f, "{line}")
        .map_err(|e| FogError::new(format!("could not write {}: {}", path.display(), e)))
}

/// Builds the `install(1)` argument vector `sudo_write` invokes to move the
/// staged file into place as root. Argument vectors are used so a path from
/// config is never interpreted by a shell.
fn sudo_write_argv(tmp: &Path, path: &Path) -> Vec<String> {
    vec![
        "install".to_string(),
        "-m".to_string(),
        "644".to_string(),
        tmp.to_string_lossy().into_owned(),
        path.to_string_lossy().into_owned(),
    ]
}

/// Caches sudo credentials once per process in interactive mode, so a run that
/// needs several privileged steps prompts only once (or not at all when
/// everything is already healthy). Headless runs never prompt: they rely on
/// `sudo -n` and report a warning if the cached ticket is missing.
fn ensure_sudo(non_interactive: bool) -> FogResult<()> {
    if non_interactive || SUDO_PRIMED.load(Ordering::Relaxed) {
        return Ok(());
    }
    if SUDO_DENIED.load(Ordering::Relaxed) {
        return Err(FogError::new(
            "sudo authentication was declined earlier in this run".to_string(),
        ));
    }
    match Command::new("sudo").arg("-v").status() {
        Ok(s) if s.success() => {
            SUDO_PRIMED.store(true, Ordering::Relaxed);
            Ok(())
        }
        Ok(s) => {
            SUDO_DENIED.store(true, Ordering::Relaxed);
            Err(FogError::new(format!("sudo authentication failed ({s})")))
        }
        Err(e) => {
            SUDO_DENIED.store(true, Ordering::Relaxed);
            Err(FogError::new(format!("sudo: {e}")))
        }
    }
}

/// Runs a privileged command, sharing the single interactive sudo prompt.
fn sudo_status(args: &[&str], non_interactive: bool) -> FogResult<()> {
    ensure_sudo(non_interactive)?;
    let mut cmd = Command::new("sudo");
    if non_interactive {
        cmd.arg("-n");
    }
    cmd.args(args);
    let status = cmd
        .status()
        .map_err(|e| FogError::new(format!("sudo: {e}")))?;
    if status.success() {
        Ok(())
    } else {
        Err(FogError::new(format!("sudo exited with {status}")))
    }
}

/// Writes `content` to `path`, escalating to `sudo` when the path is not
/// writable. In non-interactive mode sudo runs with `-n` so it cannot block on
/// a password prompt.
fn sudo_write(path: &Path, content: &str, non_interactive: bool) -> FogResult<()> {
    if let Some(parent) = path.parent()
        && !parent.exists()
    {
        sudo_status(&["mkdir", "-p", &parent.to_string_lossy()], non_interactive)?;
    }
    if fs::write(path, content).is_ok() {
        return Ok(());
    }
    // Not writable: stage a temp file in the OS temp dir and install it via
    // sudo (we cannot pipe stdin through `sudo cat`). The staging file is
    // owned by us, so it is removed without another sudo call.
    let tmp = std::env::temp_dir().join(format!(
        "fog-dnsmasq-stage-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    fs::write(&tmp, content)
        .map_err(|e| FogError::new(format!("could not stage {}: {}", tmp.display(), e)))?;
    // Run as argv, never through a shell, so a config-derived `path` cannot
    // inject commands into a root shell.
    let argv = sudo_write_argv(&tmp, path);
    let refs: Vec<&str> = argv.iter().map(String::as_str).collect();
    let res = sudo_status(&refs, non_interactive);
    let _ = fs::remove_file(&tmp);
    res
}

#[cfg(target_os = "linux")]
fn sudo_read_contains(path: &Path, needle: &str, non_interactive: bool) -> FogResult<bool> {
    // dnsmasq drop-ins are usually world-readable; reading them directly
    // avoids a sudo prompt for a check that changes nothing.
    if let Ok(content) = fs::read_to_string(path) {
        return Ok(content.lines().any(|l| l.trim() == needle));
    }
    // A missing file means "route not configured yet" — the caller will write
    // it, rather than this being treated as an error.
    let exists = run_sudo_output(&["test", "-e", &path.to_string_lossy()], non_interactive).is_ok();
    if exists {
        let out = run_sudo_output(&["cat", &path.to_string_lossy()], non_interactive)?;
        return Ok(out.lines().any(|l| l.trim() == needle));
    }
    Ok(false)
}

/// Sends a real UDP DNS query for a probe host under `domain` and checks that
/// dnsmasq answers with the configured address. A TCP connect to :53 only
/// proves a socket is listening; it says nothing about whether the daemon
/// reloaded its routes, so every repair decision is based on this query.
///
/// Non-IPv4 mappings cannot be checked this way, so they are treated as
/// unverified rather than failing every run.
fn probe_domain(address: &str, port: u16, domain: &str) -> bool {
    let Ok(expected) = address.parse::<Ipv4Addr>() else {
        return true;
    };
    let Ok(mut addrs) = format!("{address}:{port}").to_socket_addrs() else {
        return false;
    };
    let Some(target) = addrs.next() else {
        return false;
    };
    let id = next_query_id();
    let query = build_dns_query(id, &format!("fog-probe.{domain}"));
    let Ok(socket) = UdpSocket::bind("0.0.0.0:0") else {
        return false;
    };
    if socket.send_to(&query, target).is_err() {
        return false;
    }
    let mut buf = [0u8; 512];
    let deadline = Instant::now() + Duration::from_millis(700);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return false;
        }
        let _ = socket.set_read_timeout(Some(remaining));
        match socket.recv_from(&mut buf) {
            // Ignore datagrams from anything but the queried resolver: a stray
            // local responder must not make a broken daemon look healthy.
            Ok((n, from)) if from == target => return parse_dns_a(&buf[..n], id, expected),
            Ok(_) => continue,
            Err(_) => return false,
        }
    }
}

/// A fresh query id per probe, so a delayed answer cannot be mistaken for the
/// current one. The id only needs to differ between in-flight queries.
fn next_query_id() -> u16 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();
    (std::process::id() as u16) ^ (nanos as u16)
}

/// Builds a minimal recursive DNS query for an A record.
fn build_dns_query(id: u16, name: &str) -> Vec<u8> {
    let mut query = Vec::with_capacity(12 + name.len() + 6);
    query.extend_from_slice(&id.to_be_bytes());
    query.extend_from_slice(&[0x01, 0x00]); // RD=1
    query.extend_from_slice(&1u16.to_be_bytes()); // QDCOUNT
    query.extend_from_slice(&[0, 0, 0, 0, 0, 0]); // AN/NS/AR counts
    for label in name.split('.').filter(|label| !label.is_empty()) {
        query.push(label.len() as u8);
        query.extend_from_slice(label.as_bytes());
    }
    query.push(0);
    query.extend_from_slice(&1u16.to_be_bytes()); // QTYPE A
    query.extend_from_slice(&1u16.to_be_bytes()); // QCLASS IN
    query
}

/// Returns `true` when `msg` is a response to `id` that carries an A record
/// equal to `expected`. Malformed input returns `false`, never panics.
fn parse_dns_a(msg: &[u8], id: u16, expected: Ipv4Addr) -> bool {
    if msg.len() < 12 {
        return false;
    }
    if u16::from_be_bytes([msg[0], msg[1]]) != id {
        return false;
    }
    let flags = u16::from_be_bytes([msg[2], msg[3]]);
    if flags & 0x8000 == 0 || flags & 0x000F != 0 {
        return false; // not a response, or an error rcode
    }
    let qdcount = u16::from_be_bytes([msg[4], msg[5]]);
    let ancount = u16::from_be_bytes([msg[6], msg[7]]);
    if qdcount != 1 || ancount == 0 {
        return false;
    }
    let mut pos = 12;
    if skip_dns_name(msg, &mut pos).is_none() || pos + 4 > msg.len() {
        return false;
    }
    pos += 4; // QTYPE + QCLASS

    for _ in 0..ancount {
        if skip_dns_name(msg, &mut pos).is_none() || pos + 10 > msg.len() {
            return false;
        }
        let rtype = u16::from_be_bytes([msg[pos], msg[pos + 1]]);
        let class = u16::from_be_bytes([msg[pos + 2], msg[pos + 3]]);
        let rdlen = u16::from_be_bytes([msg[pos + 8], msg[pos + 9]]) as usize;
        pos += 10;
        if pos + rdlen > msg.len() {
            return false;
        }
        if rtype == 1 && class == 1 && rdlen == 4 && msg[pos..pos + 4] == expected.octets() {
            return true;
        }
        pos += rdlen;
    }
    false
}

/// Skips a DNS name at `pos`, following a compression pointer's two bytes (the
/// label data itself lives elsewhere and does not need to be visited).
fn skip_dns_name(msg: &[u8], pos: &mut usize) -> Option<()> {
    for _ in 0..128 {
        let len = *msg.get(*pos)? as usize;
        if len == 0 {
            *pos += 1;
            return Some(());
        }
        if len & 0xC0 == 0xC0 {
            *pos += 2;
            return Some(());
        }
        if len & 0xC0 != 0 {
            return None;
        }
        *pos += 1 + len;
        if *pos > msg.len() {
            return None;
        }
    }
    None
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
/// changed, whether the daemon is up, and whether it answers a real query:
///
/// * `start` — the daemon is down; bring it up.
/// * `restart` — the daemon is up but its config changed or it does not
///   resolve the configured domains (a stale route or a failed previous
///   reload); reload it.
/// * `None` — the daemon is up and every configured domain resolves.
///
/// Split out so the decision logic is pure and testable.
fn next_service_action(changed: bool, running: bool, resolving: bool) -> Option<&'static str> {
    if !running {
        Some("start")
    } else if changed || !resolving {
        Some("restart")
    } else {
        None
    }
}

/// Ensures the dnsmasq daemon is running (starting it if it is down) and
/// restarts it when the config changed **or a configured domain does not
/// resolve**. A previously failed restart is therefore repaired on the next
/// run instead of being skipped because the files look unchanged.
fn ensure_running(
    changed: bool,
    address: &str,
    port: u16,
    domains: &[String],
    non_interactive: bool,
    messages: &mut Vec<String>,
) {
    let tcp_up = is_running(address, port);
    let resolving = !tcp_up || domains.iter().all(|d| probe_domain(address, port, d));
    let Some(action) = next_service_action(changed, tcp_up, resolving) else {
        flush_stale_system_cache(address, port, domains, non_interactive, messages);
        return;
    };

    service_dnsmasq(action, non_interactive, messages);

    let resolving = wait_for_dns(address, port, domains, Duration::from_secs(3));

    // `brew services` can report success while the loaded root LaunchDaemon is
    // untouched (Homebrew does not see a daemon it did not load). Kick the
    // actual launchd job and probe again before declaring failure.
    #[cfg(target_os = "macos")]
    let resolving = if !resolving && kickstart_dnsmasq(non_interactive, messages) {
        wait_for_dns(address, port, domains, Duration::from_secs(2))
    } else {
        resolving
    };

    if resolving {
        messages.push(format!(
            "  + dnsmasq is up on {address}:{port} and resolving all configured domains"
        ));
        flush_dns_cache(non_interactive);
    } else if is_running(address, port) {
        messages.push(format!(
            "⚠ dnsmasq is listening on {address}:{port} but did not answer for {}. \
             Existing routes may be stale; run `fog <script>` interactively once.",
            domains.join(", ")
        ));
    } else {
        messages.push(
            "⚠ dnsmasq did not come up after starting; check `brew services list dnsmasq` \
             or system logs."
                .to_string(),
        );
    }
}

/// Polls dnsmasq with real queries until every domain answers or `timeout`
/// expires.
fn wait_for_dns(address: &str, port: u16, domains: &[String], timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if is_running(address, port) && domains.iter().all(|d| probe_domain(address, port, d)) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(150));
    }
}

/// Drops the system resolver's negative cache after a repair. Without this a
/// host that failed before the fix can keep returning NXDOMAIN in Chrome even
/// though dnsmasq now answers.
#[cfg(target_os = "macos")]
fn flush_dns_cache(non_interactive: bool) {
    // Bound the flush: a wedged resolver must not hang a detached startup,
    // which cannot even be interrupted from a TUI.
    let mut flush = Command::new("dscacheutil");
    flush.arg("-flushcache");
    let _ = run_with_timeout(flush, Duration::from_secs(2));
    let _ = run_sudo(&["killall", "-HUP", "mDNSResponder"], non_interactive);
}

/// Even with a healthy dnsmasq and a correct `/etc/resolver` file, macOS can
/// keep serving a cached negative answer. Check the system resolver itself and
/// flush only when it disagrees with dnsmasq, so a healthy start asks for no
/// password.
fn flush_stale_system_cache(
    address: &str,
    port: u16,
    domains: &[String],
    non_interactive: bool,
    messages: &mut Vec<String>,
) {
    #[cfg(target_os = "macos")]
    {
        // /etc/resolver files written by fog carry no port, so macOS always
        // queries :53. A daemon on another port cannot be validated through
        // the system resolver; checking it would flush and warn every start.
        if port != 53 {
            return;
        }
        if domains.iter().all(|d| system_resolves(address, d)) {
            return;
        }
        messages.push("  + dnsmasq answers; flush a stale macOS DNS cache".to_string());
        flush_dns_cache(non_interactive);
        for domain in domains {
            if !system_resolves(address, domain) {
                messages.push(format!(
                    "⚠ the system resolver still does not resolve {domain}; \
                     check `/etc/resolver/{domain}` and run \
                     `sudo killall -HUP mDNSResponder`."
                ));
            }
        }
    }
    #[cfg(not(target_os = "macos"))]
    let _ = (address, port, domains, non_interactive, messages);
}

/// Resolves a probe name through the OS resolver stack, which honours
/// `/etc/resolver`, unlike a direct query to dnsmasq.
#[cfg(target_os = "macos")]
fn system_resolves(address: &str, domain: &str) -> bool {
    let mut cmd = Command::new("dscacheutil");
    cmd.args(["-q", "host", "-a", "name", &format!("fog-probe.{domain}")]);
    // mDNSResponder can block behind an unreachable resolver. The probe is a
    // health check, so a slow answer is treated as "not resolved" instead of
    // stalling startup forever.
    let Some(output) = run_with_timeout(cmd, Duration::from_secs(5)) else {
        return false;
    };
    output.status.success() && output_has_ipv4(&String::from_utf8_lossy(&output.stdout), address)
}

#[cfg(target_os = "macos")]
fn output_has_ipv4(text: &str, address: &str) -> bool {
    text.lines().any(|line| {
        line.trim()
            .strip_prefix("ip_address:")
            .is_some_and(|ip| ip.trim() == address)
    })
}

#[cfg(not(target_os = "macos"))]
fn flush_dns_cache(_non_interactive: bool) {}

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
        match sudo_status(&["systemctl", action, "dnsmasq"], non_interactive) {
            Ok(()) => messages.push(format!(
                "  + dnsmasq {action}ed (sudo systemctl {action} dnsmasq)"
            )),
            Err(_) => {
                messages.push(format!(
                    "⚠ could not {action} dnsmasq (sudo systemctl {action} dnsmasq). Existing \
                     routes may not be active yet."
                ));
            }
        }
    }

    #[cfg(target_os = "macos")]
    {
        // A stale user-level brew Agent (from the pre-root era) would fail to
        // bind :53 and could crash-loop or shadow the daemon. Remove it first;
        // this is our own GUI domain, so it needs no root.
        let _ = bootout_user_agent();

        // Running `brew services` as root installs a LaunchDaemon in
        // /Library/LaunchDaemons so dnsmasq runs as root and can bind :53.
        match sudo_status(&["brew", "services", action, "dnsmasq"], non_interactive) {
            Ok(()) => messages.push(format!(
                "  + dnsmasq {action}ed (sudo brew services {action} dnsmasq)"
            )),
            Err(_) => {
                messages.push(format!(
                    "⚠ could not {action} dnsmasq (sudo brew services {action} dnsmasq). Run \
                     `fog <script>` interactively once and approve the sudo prompt. Existing \
                     routes may not be active yet."
                ));
            }
        }
    }
}

/// Restarts the loaded root LaunchDaemon directly when `brew services` left it
/// untouched. On machines where dnsmasq was loaded by `brew services` under
/// root, Homebrew itself reports it as "not loaded", so its restart command can
/// succeed without reloading anything.
#[cfg(target_os = "macos")]
fn kickstart_dnsmasq(non_interactive: bool, messages: &mut Vec<String>) -> bool {
    for label in ["system/sh.brew.dnsmasq", "system/homebrew.mxcl.dnsmasq"] {
        let loaded = Command::new("launchctl")
            .args(["print", label])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !loaded {
            continue;
        }
        if sudo_status(&["launchctl", "kickstart", "-k", label], non_interactive).is_ok() {
            messages.push(format!("  + restarted dnsmasq through launchctl ({label})"));
            return true;
        }
    }
    false
}

/// Boots out any user-level `homebrew.mxcl.dnsmasq` LaunchAgent so it cannot
/// compete with (or shadow) the root LaunchDaemon draining :53. The agent
/// lives in the current user's GUI domain, so no sudo is needed.
#[cfg(target_os = "macos")]
fn bootout_user_agent() -> FogResult<()> {
    let domain = format!("gui/{}", unsafe { libc::getuid() });
    // launchctl takes a single service target (`gui/<uid>/<label>`), not a
    // domain followed by a separate label argument.
    let target = launchctl_service_target(&domain, "homebrew.mxcl.dnsmasq");
    // Only try to unload if it is actually registered; otherwise bootout fails
    // with a confusing error.
    let loaded = Command::new("launchctl")
        .args(["print", &target])
        .output()
        .ok()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !loaded {
        return Ok(());
    }
    let status = Command::new("launchctl")
        .args(["bootout", &target])
        .status()
        .map_err(|e| FogError::new(format!("launchctl: {e}")))?;
    if status.success() {
        Ok(())
    } else {
        Err(FogError::new(
            "launchctl bootout homebrew.mxcl.dnsmasq failed".to_string(),
        ))
    }
}

#[cfg(target_os = "macos")]
fn launchctl_service_target(domain: &str, label: &str) -> String {
    format!("{domain}/{label}")
}

#[cfg(target_os = "macos")]
fn run_sudo(args: &[&str], non_interactive: bool) -> FogResult<()> {
    sudo_status(args, non_interactive)
}

#[cfg(target_os = "linux")]
fn run_sudo_output(args: &[&str], non_interactive: bool) -> FogResult<String> {
    ensure_sudo(non_interactive)?;
    let mut cmd = Command::new("sudo");
    if non_interactive {
        cmd.arg("-n");
    }
    cmd.args(args);
    let out = cmd
        .output()
        .map_err(|e| FogError::new(format!("sudo: {e}")))?;
    if !out.status.success() {
        return Err(FogError::new(format!("sudo exited with {}", out.status)));
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
    #[ignore = "calls the real dnsmasq setup and can change host DNS state"]
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
        assert_eq!(next_service_action(false, false, true), Some("start"));
        assert_eq!(next_service_action(true, false, true), Some("start"));
    }

    #[test]
    fn test_next_action_restarts_when_changed_and_up() {
        assert_eq!(next_service_action(true, true, true), Some("restart"));
    }

    #[test]
    fn test_next_action_restarts_when_dns_does_not_resolve() {
        assert_eq!(
            next_service_action(false, true, false),
            Some("restart"),
            "a listening daemon that does not answer must be repaired"
        );
    }

    #[test]
    fn test_next_action_noop_when_up_and_resolving() {
        assert_eq!(next_service_action(false, true, true), None);
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
    fn test_validate_domain_rejects_shell_injection() {
        let err = validate_domain("x'; touch /tmp/pwned; '")
            .unwrap_err()
            .to_string();
        assert!(err.contains("invalid domain"), "unexpected error: {err}");
        assert!(validate_domain("../etc/passwd").is_err());
        assert!(validate_domain("").is_err());
    }

    #[test]
    fn test_validate_domain_accepts_hostname() {
        assert!(validate_domain("red-fox").is_ok());
        assert!(validate_domain("dev.local").is_ok());
        assert!(validate_domain("a1.b2-c3").is_ok());
    }

    #[test]
    fn test_sudo_write_argv_is_not_a_shell_command() {
        let argv = sudo_write_argv(
            Path::new("/tmp/fog-stage-1"),
            Path::new("/etc/resolver/red-fox"),
        );
        assert!(
            !argv.iter().any(|a| a == "sh" || a == "-c"),
            "argv must not invoke a shell: {argv:?}"
        );
        assert_eq!(argv[0], "install");
        assert_eq!(
            argv.last().map(String::as_str),
            Some("/etc/resolver/red-fox")
        );
        assert!(
            !argv.iter().any(|a| a == "rm"),
            "the user-owned staging file must not need a second sudo call: {argv:?}"
        );
    }

    #[test]
    fn test_probe_domain_ignores_a_reply_from_another_socket() {
        let responder = UdpSocket::bind("127.0.0.1:0").unwrap();
        let decoy = UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = responder.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            let mut buf = [0u8; 512];
            let (n, from) = responder.recv_from(&mut buf).unwrap();
            // A wrong-answer datagram from another socket must be skipped,
            // then the real resolver's answer must be accepted.
            let wrong = a_response(&buf[..n], [10, 0, 0, 1]);
            let _ = decoy.send_to(&wrong, from);
            std::thread::sleep(Duration::from_millis(20));
            let response = a_response(&buf[..n], [127, 0, 0, 1]);
            let _ = responder.send_to(&response, from);
        });
        assert!(probe_domain("127.0.0.1", port, "red-fox"));
        handle.join().unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn test_launchctl_service_target_is_one_argument() {
        assert_eq!(
            launchctl_service_target("gui/501", "homebrew.mxcl.dnsmasq"),
            "gui/501/homebrew.mxcl.dnsmasq",
            "launchctl print/bootout take domain/label as one service target"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn test_dscacheutil_output_matches_the_exact_ipv4() {
        let text = "name: fog-probe.red-fox\nip_address: 127.0.0.1\n";
        assert!(output_has_ipv4(text, "127.0.0.1"));
        assert!(
            !output_has_ipv4(text, "127.0.0.10"),
            "a longer address must not match by prefix"
        );
        assert!(!output_has_ipv4("name: fog-probe.red-fox\n", "127.0.0.1"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn test_system_cache_check_skips_a_nonstandard_port() {
        // /etc/resolver has no port, so a non-:53 daemon is not reachable
        // through dscacheutil. The check must return without flushing,
        // prompting, or warning on every healthy start.
        let mut messages = Vec::new();
        flush_stale_system_cache(
            "127.0.0.1",
            5353,
            &["red-fox".to_string()],
            false,
            &mut messages,
        );
        assert!(
            messages.is_empty(),
            "a non-:53 daemon must not trigger a cache repair: {messages:?}"
        );
    }

    /// Builds a valid response to `query` with one A record.
    fn a_response(query: &[u8], ip: [u8; 4]) -> Vec<u8> {
        let mut response = query.to_vec();
        response[2] = 0x81; // QR=1, RD=1
        response[3] = 0x80; // RA=1, rcode=0
        response[6] = 0;
        response[7] = 1; // ANCOUNT
        response.extend_from_slice(&[0xC0, 0x0C, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4]);
        response.extend_from_slice(&ip);
        response
    }

    #[test]
    fn test_build_dns_query_encodes_labels_and_a_record() {
        let query = build_dns_query(0x1234, "fog-probe.red-fox");
        assert_eq!(&query[..2], &[0x12, 0x34]);
        assert_eq!(&query[4..6], &[0, 1], "one question");
        assert_eq!(query[12], 9);
        assert_eq!(&query[13..22], b"fog-probe");
        assert_eq!(query[22], 7);
        assert_eq!(&query[23..30], b"red-fox");
        assert_eq!(query[30], 0);
        assert_eq!(&query[31..35], &[0, 1, 0, 1], "QTYPE A / QCLASS IN");
    }

    #[test]
    fn test_parse_dns_a_accepts_the_configured_address() {
        let query = build_dns_query(7, "fog-probe.red-fox");
        let response = a_response(&query, [127, 0, 0, 1]);
        assert!(parse_dns_a(&response, 7, Ipv4Addr::new(127, 0, 0, 1)));
        assert!(
            !parse_dns_a(&response, 7, Ipv4Addr::new(10, 0, 0, 1)),
            "an answer for another address must not count"
        );
    }

    #[test]
    fn test_parse_dns_a_rejects_wrong_id_and_error_rcode() {
        let query = build_dns_query(7, "fog-probe.red-fox");
        let response = a_response(&query, [127, 0, 0, 1]);
        assert!(!parse_dns_a(&response, 8, Ipv4Addr::new(127, 0, 0, 1)));
        assert!(!parse_dns_a(&[0u8; 3], 7, Ipv4Addr::new(127, 0, 0, 1)));

        let mut nxdomain = response.clone();
        nxdomain[3] = 0x83; // rcode 3
        assert!(!parse_dns_a(&nxdomain, 7, Ipv4Addr::new(127, 0, 0, 1)));
    }

    #[test]
    fn test_probe_domain_round_trips_a_udp_query() {
        let server = UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = server.local_addr().unwrap().port();
        let join = std::thread::spawn(move || {
            let mut buf = [0u8; 512];
            let (n, from) = server.recv_from(&mut buf).unwrap();
            let response = a_response(&buf[..n], [127, 0, 0, 1]);
            server.send_to(&response, from).unwrap();
        });
        assert!(probe_domain("127.0.0.1", port, "red-fox"));
        join.join().unwrap();
    }

    #[cfg(target_os = "macos")]
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

    #[cfg(target_os = "macos")]
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
