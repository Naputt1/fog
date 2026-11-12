//! `fog logs` and its line windowing: list an instance's services or print one
//! service's captured output, optionally cut with `--head`/`--tail`.
//!
//! A finished instance has no socket, so its logs are read straight from the
//! per-instance log directory; a running instance's logs are read the same way,
//! except the proxy's request log, which streams over IPC and only lives in
//! memory.

use crate::cli::presentation::{color_enabled, error_label, service_table, strip_ansi};
use crate::cli::{Cli, die};
use crate::error::{FogError, FogResult};
use crate::ipc;
use crate::service_state::ServiceState;
use std::fs;
use std::io::{self, BufRead, BufReader};
use std::path::Path;

use super::resolve_targets;

/// Builds the `(service, status)` rows for `fog logs`: one per script service
/// (`healthy`/`unhealthy`/… while running, `stopped` otherwise), plus `daemon`
/// when its log file exists and `proxy` when one is configured.
fn log_service_rows(status: &ipc::StatusResponse, daemon_exists: bool) -> Vec<(String, String)> {
    let mut rows: Vec<(String, String)> = status
        .services
        .iter()
        .map(|s| {
            let state = if s.running {
                s.health.clone()
            } else {
                ServiceState::Stopped.as_str().to_string()
            };
            (s.name.clone(), state)
        })
        .collect();
    if daemon_exists {
        rows.push((
            "daemon".to_string(),
            ServiceState::Running.as_str().to_string(),
        ));
    }
    if let Some(p) = &status.proxy {
        let state = if p.running {
            format!(":{}", p.port)
        } else {
            ":down".to_string()
        };
        rows.push(("proxy".to_string(), state));
    }
    rows
}

/// A `--head`/`--tail` window in the spirit of `head -n` / `tail -n`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LineRange {
    /// First `n` lines (`--head N`).
    First(usize),
    /// All but the last `n` lines (`--head -N`).
    ExceptLast(usize),
    /// Last `n` lines (`--tail N` / `--tail -N`).
    Last(usize),
    /// From 1-indexed line `n` to the end (`--tail +N`).
    From(usize),
}

/// Parses `--head`: `N` (first N) or `-N` (all but the last N).
pub(crate) fn parse_head_spec(s: &str) -> FogResult<LineRange> {
    let (digits, make): (&str, fn(usize) -> LineRange) = match s.strip_prefix('-') {
        Some(rest) => (rest, LineRange::ExceptLast),
        None => (s, LineRange::First),
    };
    digits
        .parse::<usize>()
        .map(make)
        .map_err(|_| FogError::new(format!("invalid --head value '{s}': expected N or -N")))
}

/// Parses `--tail`: `N`/`-N` (last N) or `+N` (from line N to the end).
pub(crate) fn parse_tail_spec(s: &str) -> FogResult<LineRange> {
    let (digits, make): (&str, fn(usize) -> LineRange) = match s.strip_prefix('+') {
        Some(rest) => (rest, LineRange::From),
        None => (s.strip_prefix('-').unwrap_or(s), LineRange::Last),
    };
    digits
        .parse::<usize>()
        .map(make)
        .map_err(|_| FogError::new(format!("invalid --tail value '{s}': expected N, -N or +N")))
}

/// Resolves a window against a file of `total` lines into a half-open
/// `[start, end)` line index range.
fn resolve_range(range: LineRange, total: usize) -> (usize, usize) {
    match range {
        LineRange::First(n) => (0, n.min(total)),
        LineRange::ExceptLast(n) => (0, total.saturating_sub(n)),
        LineRange::Last(n) => (total.saturating_sub(n), total),
        LineRange::From(n) => (n.saturating_sub(1).min(total), total),
    }
}

/// Resolves `--head`/`--tail` into ordered, non-overlapping line segments.
/// `--head N --tail M` yields the first N and last M lines as two segments
/// (the caller prints an elision marker between them); overlapping windows
/// merge into one.
fn line_segments(
    head: Option<LineRange>,
    tail: Option<LineRange>,
    total: usize,
) -> Vec<(usize, usize)> {
    let mut segs: Vec<(usize, usize)> = Vec::new();
    if let Some(h) = head {
        segs.push(resolve_range(h, total));
    }
    if let Some(t) = tail {
        segs.push(resolve_range(t, total));
    }
    segs.retain(|(start, end)| end > start);
    segs.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(segs.len());
    for (start, end) in segs {
        match merged.last_mut() {
            Some(last) if start <= last.1 => last.1 = last.1.max(end),
            _ => merged.push((start, end)),
        }
    }
    merged
}

/// The marker printed between two disjoint selected segments.
fn omitted_marker(n: usize) -> String {
    format!("... {n} lines omitted ...")
}

/// Applies `head`/`tail` to an in-memory slice of lines (used for the proxy
/// request log), inserting the elision marker between disjoint segments.
fn select_lines(lines: &[String], head: Option<LineRange>, tail: Option<LineRange>) -> Vec<String> {
    let segs = line_segments(head, tail, lines.len());
    let mut out = Vec::new();
    for (i, &(start, end)) in segs.iter().enumerate() {
        if i > 0 {
            let omitted = start - segs[i - 1].1;
            if omitted > 0 {
                out.push(omitted_marker(omitted));
            }
        }
        out.extend_from_slice(&lines[start..end]);
    }
    out
}

/// Counts newline-terminated (or final partial) lines in a log file without
/// holding it in memory.
fn count_log_lines(file: &Path) -> io::Result<usize> {
    let mut reader = BufReader::new(fs::File::open(file)?);
    let mut buf = Vec::new();
    let mut count = 0usize;
    loop {
        buf.clear();
        if reader.read_until(b'\n', &mut buf)? == 0 {
            return Ok(count);
        }
        count += 1;
    }
}

/// Streams only the selected line segments of a log file, emitting the
/// elision marker between them. ANSI sequences are stripped and trailing
/// `\r` removed, matching the whole-file path.
fn print_log_slice(
    file: &Path,
    head: Option<LineRange>,
    tail: Option<LineRange>,
) -> io::Result<()> {
    let total = count_log_lines(file)?;
    let segs = line_segments(head, tail, total);
    if segs.is_empty() {
        return Ok(());
    }
    let mut reader = BufReader::new(fs::File::open(file)?);
    let mut buf = Vec::new();
    let mut idx = 0usize;
    for (i, &(start, end)) in segs.iter().enumerate() {
        while idx < start {
            buf.clear();
            if reader.read_until(b'\n', &mut buf)? == 0 {
                return Ok(());
            }
            idx += 1;
        }
        if i > 0 {
            let omitted = start - segs[i - 1].1;
            if omitted > 0 {
                println!("{}", omitted_marker(omitted));
            }
        }
        while idx < end {
            buf.clear();
            if reader.read_until(b'\n', &mut buf)? == 0 {
                return Ok(());
            }
            let text = String::from_utf8_lossy(&buf);
            println!("{}", strip_ansi(text.trim_end_matches(['\r', '\n'])));
            idx += 1;
        }
    }
    Ok(())
}

/// Prints one captured log file as a single `==== <script> (<name>) ====`
/// section. With `head`/`tail` set, only the selected line window is printed
/// (ANSI stripped); otherwise the whole file is printed as before.
fn print_log_file(
    script: &str,
    name: &str,
    file: &Path,
    head: Option<LineRange>,
    tail: Option<LineRange>,
) {
    println!("==== {} ({}) ====", script, name);
    if head.is_none() && tail.is_none() {
        match fs::read_to_string(file) {
            Ok(content) => {
                print!("{}", strip_ansi(&content));
                if !content.ends_with('\n') {
                    println!();
                }
            }
            Err(e) => die(&format!("could not read {}: {e}", file.display()), None, 1),
        }
        return;
    }
    if let Err(e) = print_log_slice(file, head, tail) {
        die(&format!("could not read {}: {e}", file.display()), None, 1);
    }
}

/// Lists the instance's services and their status, with a hint pointing at
/// `--service`. Used when `fog logs` runs without a service filter and when
/// reporting an unknown `--service` name.
fn print_available_services(target_pid: u32, script: Option<&str>, rows: &[(String, String)]) {
    if rows.is_empty() {
        match script {
            Some(_) => println!("(no services for instance {target_pid})"),
            None => println!("(no captured logs for instance {target_pid})"),
        }
        return;
    }
    match script {
        Some(script) => println!("Services for instance {target_pid} (script '{script}'):"),
        None => println!("Captured logs of finished instance {target_pid} (not running):"),
    }
    for line in service_table(rows, "  ", color_enabled()) {
        println!("{line}");
    }
    println!();
    println!("Show one service: fog logs {target_pid} --service <name>");
}

/// Lists the captured log files of a finished instance as `(service, state)`
/// rows. A finished run has no live status, so every row reads `finished`.
fn finished_log_rows(dir: &Path) -> Vec<(String, String)> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name();
            let name = name.to_str()?.strip_suffix(".log")?;
            entry.path().is_file().then(|| name.to_string())
        })
        .collect();
    names.sort();
    names
        .into_iter()
        .map(|name| (name, "finished".to_string()))
        .collect()
}

/// Resolves a user-supplied service name to the instance's canonical name:
/// an exact match first, then a sanitized filename-stem match (so a service
/// whose name contains `/` etc. is found by either form), and finally a
/// `<name>.log` on disk, so a stale log left by a removed service stays
/// printable by name.
fn resolve_service_name(dir: &Path, rows: &[(String, String)], wanted: &str) -> Option<String> {
    if let Some(name) = rows
        .iter()
        .map(|(name, _)| name.as_str())
        .find(|name| *name == wanted)
    {
        return Some(name.to_string());
    }
    let safe = ipc::sanitize_service_name(wanted);
    if let Some(name) = rows
        .iter()
        .map(|(name, _)| name.as_str())
        .find(|name| ipc::sanitize_service_name(name) == safe || *name == safe)
    {
        return Some(name.to_string());
    }
    dir.join(format!("{safe}.log"))
        .is_file()
        .then(|| wanted.to_string())
}

/// Prints the captured logs of a running instance.
///
/// Without `service`, lists the available service names and their status.
/// With `service`, prints only that service's captured output (`daemon` reads
/// `daemon.log`; `proxy` streams the live request log over IPC; anything else
/// reads `<service>.log`). `--head`/`--tail` limit which lines are printed.
pub(crate) fn cmd_logs(pid: Option<u32>, service: Option<String>, cli: &Cli) -> io::Result<()> {
    let instances = ipc::find_instances()?;

    // A finished instance has no socket and no live status, but its captured
    // logs stay on disk until a prune: read those directly.
    if let Some(pid) = pid {
        if !instances.iter().any(|(p, _)| *p == pid) {
            return cmd_finished_logs(pid, service.as_deref(), cli);
        }
    } else if instances.is_empty() {
        die(
            "no running fog instances",
            Some("start one with `fog <script>`"),
            1,
        );
    }

    let targets = resolve_targets(&instances, pid, false, cli, "logs");
    let (target_pid, path) = (targets[0].0, &targets[0].1);

    let status = match ipc::query_status(path) {
        Ok(s) => s,
        Err(e) => die(
            &format!("could not query instance {target_pid}: {e}"),
            None,
            1,
        ),
    };

    let dir = ipc::instance_log_dir(target_pid);
    let rows = log_service_rows(&status, dir.join("daemon.log").is_file());

    let Some(wanted) = service.as_deref() else {
        print_available_services(target_pid, Some(&status.script), &rows);
        return Ok(());
    };

    let Some(name) = resolve_service_name(&dir, &rows, wanted) else {
        eprintln!(
            "{} unknown service '{wanted}' for instance {target_pid}",
            error_label()
        );
        print_available_services(target_pid, Some(&status.script), &rows);
        std::process::exit(1);
    };

    if name == "proxy" {
        // The proxy log lives in a bounded in-memory queue; fetch enough to
        // resolve any window, but let a plain `--tail N` fetch just N.
        let fetch = match (cli.head, cli.tail) {
            (None, Some(LineRange::Last(n))) => n.max(1),
            _ => 10_000,
        };
        match ipc::query_logs(path, "proxy", fetch) {
            Ok(lines) => {
                println!("==== {} (proxy) ====", status.script);
                if cli.head.is_none() && cli.tail.is_none() {
                    for line in lines {
                        println!("{line}");
                    }
                } else {
                    for line in select_lines(&lines, cli.head, cli.tail) {
                        println!("{line}");
                    }
                }
            }
            Err(e) => {
                die(
                    &format!("could not query proxy log on instance {target_pid}: {e}"),
                    None,
                    1,
                );
            }
        }
        return Ok(());
    }

    let file = dir.join(format!("{}.log", ipc::sanitize_service_name(&name)));
    if !file.is_file() {
        die(
            &format!("instance {target_pid} has no captured log for service '{name}'"),
            Some("run `fog logs <pid>` to list services with logs"),
            1,
        );
    }
    print_log_file(&status.script, &name, &file, cli.head, cli.tail);
    Ok(())
}

/// Prints the captured logs of an instance that is no longer running, using the
/// files left in its log directory.
fn cmd_finished_logs(pid: u32, service: Option<&str>, cli: &Cli) -> io::Result<()> {
    let dir = ipc::instance_log_dir(pid);
    if !dir.is_dir() {
        die(
            &format!("no running instance and no captured logs for pid {pid}"),
            Some("run `fog ls` to list running instances"),
            1,
        );
    }

    let rows = finished_log_rows(&dir);
    let Some(wanted) = service else {
        print_available_services(pid, None, &rows);
        return Ok(());
    };

    if wanted == "proxy" {
        die(
            "the proxy request log is held in memory and is gone once the instance exits",
            Some("the other services' logs are still readable"),
            1,
        );
    }

    let Some(name) = resolve_service_name(&dir, &rows, wanted) else {
        eprintln!(
            "{} unknown service '{wanted}' for finished instance {pid}",
            error_label()
        );
        print_available_services(pid, None, &rows);
        std::process::exit(1);
    };

    let file = dir.join(format!("{}.log", ipc::sanitize_service_name(&name)));
    if !file.is_file() {
        die(
            &format!("finished instance {pid} has no captured log for service '{name}'"),
            Some("run `fog logs <pid>` to list services with logs"),
            1,
        );
    }
    print_log_file(
        &format!("finished run {pid}"),
        &name,
        &file,
        cli.head,
        cli.tail,
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::commands::test_status;
    use crate::cli::temp_dir;

    fn svc(name: &str, running: bool, health: &str) -> ipc::ServiceStatus {
        ipc::ServiceStatus {
            name: name.to_string(),
            running,
            health: health.to_string(),
            endpoints: Vec::new(),
            shutdown_cmd: None,
        }
    }

    #[test]
    fn test_log_service_rows_reports_health_and_stopped() {
        let status = test_status(
            vec![svc("api", true, "healthy"), svc("worker", false, "unknown")],
            None,
        );
        assert_eq!(
            log_service_rows(&status, false),
            vec![
                ("api".to_string(), "healthy".to_string()),
                ("worker".to_string(), "stopped".to_string()),
            ]
        );
    }

    #[test]
    fn test_log_service_rows_appends_daemon_and_proxy() {
        let status = test_status(
            vec![svc("api", true, "healthy")],
            Some(ipc::ProxyStatus {
                running: true,
                port: 3000,
            }),
        );
        assert_eq!(
            log_service_rows(&status, true),
            vec![
                ("api".to_string(), "healthy".to_string()),
                ("daemon".to_string(), "running".to_string()),
                ("proxy".to_string(), ":3000".to_string()),
            ]
        );
        // No daemon.log on disk and no proxy configured: no extra rows.
        let status = test_status(vec![svc("api", true, "healthy")], None);
        assert_eq!(
            log_service_rows(&status, false),
            vec![("api".to_string(), "healthy".to_string())]
        );
    }

    #[test]
    fn test_log_service_rows_proxy_down() {
        let status = test_status(
            Vec::new(),
            Some(ipc::ProxyStatus {
                running: false,
                port: 3000,
            }),
        );
        assert_eq!(
            log_service_rows(&status, false),
            vec![("proxy".to_string(), ":down".to_string())]
        );
    }

    #[test]
    fn test_finished_log_rows_lists_log_files_sorted() {
        let dir = temp_dir();
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("worker.log"), "w").unwrap();
        fs::write(dir.join("api.log"), "a").unwrap();
        fs::write(dir.join("daemon.log"), "d").unwrap();
        fs::write(dir.join("startup.progress"), "x").unwrap();
        fs::create_dir_all(dir.join("nested.log")).unwrap();

        let rows = finished_log_rows(&dir);

        assert_eq!(
            rows,
            vec![
                ("api".to_string(), "finished".to_string()),
                ("daemon".to_string(), "finished".to_string()),
                ("worker".to_string(), "finished".to_string()),
            ],
            "only *.log files, sorted, with no live status"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_finished_log_rows_missing_dir_is_empty() {
        let dir = temp_dir().join("gone");
        assert!(finished_log_rows(&dir).is_empty());
    }

    #[test]
    fn test_resolve_service_name_finds_listed_and_stale_logs() {
        let dir = temp_dir();
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("orphan.log"), "x").unwrap();
        let rows = vec![
            ("api".to_string(), "finished".to_string()),
            ("a/b".to_string(), "finished".to_string()),
        ];

        assert_eq!(
            resolve_service_name(&dir, &rows, "api").as_deref(),
            Some("api")
        );
        // Sanitized form of a name containing `/` maps back to the row.
        assert_eq!(
            resolve_service_name(&dir, &rows, "a_b").as_deref(),
            Some("a/b")
        );
        // Not listed, but its log file is on disk: still printable by name.
        assert_eq!(
            resolve_service_name(&dir, &[], "orphan").as_deref(),
            Some("orphan")
        );
        assert_eq!(resolve_service_name(&dir, &[], "nope"), None);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_parse_head_spec() {
        assert_eq!(parse_head_spec("50").unwrap(), LineRange::First(50));
        assert_eq!(parse_head_spec("-50").unwrap(), LineRange::ExceptLast(50));
        // A leading '+' is accepted as the plain count.
        assert_eq!(parse_head_spec("+50").unwrap(), LineRange::First(50));
        assert!(parse_head_spec("x").is_err());
    }

    #[test]
    fn test_parse_tail_spec() {
        assert_eq!(parse_tail_spec("50").unwrap(), LineRange::Last(50));
        assert_eq!(parse_tail_spec("-50").unwrap(), LineRange::Last(50));
        assert_eq!(parse_tail_spec("+51").unwrap(), LineRange::From(51));
        assert!(parse_tail_spec("x").is_err());
    }

    #[test]
    fn test_line_segments_bookends_and_merge() {
        assert_eq!(
            line_segments(Some(LineRange::First(5)), Some(LineRange::Last(10)), 100),
            vec![(0, 5), (90, 100)]
        );
        // Overlapping windows merge into the whole file.
        assert_eq!(
            line_segments(Some(LineRange::First(60)), Some(LineRange::Last(60)), 100),
            vec![(0, 100)]
        );
        // Touching windows (end == start) merge too.
        assert_eq!(
            line_segments(Some(LineRange::First(50)), Some(LineRange::Last(50)), 100),
            vec![(0, 100)]
        );
    }

    #[test]
    fn test_line_segments_single_and_signed() {
        assert_eq!(
            line_segments(Some(LineRange::First(5)), None, 100),
            vec![(0, 5)]
        );
        assert_eq!(
            line_segments(None, Some(LineRange::From(51)), 100),
            vec![(50, 100)]
        );
        assert_eq!(
            line_segments(Some(LineRange::ExceptLast(10)), None, 100),
            vec![(0, 90)]
        );
        // Windows larger than the file clamp to everything.
        assert_eq!(
            line_segments(Some(LineRange::First(200)), None, 100),
            vec![(0, 100)]
        );
        // A zero window selects nothing.
        assert_eq!(line_segments(Some(LineRange::First(0)), None, 100), vec![]);
        assert_eq!(line_segments(None, Some(LineRange::Last(0)), 100), vec![]);
    }

    #[test]
    fn test_select_lines_inserts_omission_marker() {
        let lines: Vec<String> = (1..=100).map(|i| i.to_string()).collect();
        let got = select_lines(&lines, Some(LineRange::First(2)), Some(LineRange::Last(2)));
        assert_eq!(got, vec!["1", "2", "... 96 lines omitted ...", "99", "100"]);
        // No marker for a single window.
        assert_eq!(
            select_lines(&lines, Some(LineRange::First(2)), None),
            vec!["1", "2"]
        );
        // Full overlap prints the whole file with no marker.
        assert_eq!(
            select_lines(&lines, None, Some(LineRange::Last(100))).len(),
            100
        );
    }

    #[test]
    fn test_count_log_lines_includes_partial_last_line() {
        let dir = temp_dir();
        let file = dir.join("count.log");
        fs::write(&file, "a\nb\nc").unwrap();
        assert_eq!(count_log_lines(&file).unwrap(), 3);
        fs::write(&file, "a\n").unwrap();
        assert_eq!(count_log_lines(&file).unwrap(), 1);
        fs::write(&file, "").unwrap();
        assert_eq!(count_log_lines(&file).unwrap(), 0);
        let _ = fs::remove_dir_all(&dir);
    }
}
