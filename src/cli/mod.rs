//! Command-line interface: argument parsing and the `fog` subcommands.
//!
//! Dispatch lives here — [`Cli`] is the clap surface, [`run`] routes it to a
//! command module, and [`die`]/[`fail`]/[`usage_error`] own the exit paths. The
//! presentation, progress, and command bodies live in the child modules.

#![deny(unsafe_op_in_unsafe_fn)]

mod commands;
mod presentation;
mod progress;

use crate::completion::CompletionShell;
use crate::config::Config;
use crate::error::FogError;
use clap::parser::ValueSource;
use clap::{CommandFactory, FromArgMatches, Parser};
use commands::attach::cmd_attach;
use commands::daemon::daemonize;
use commands::kill::cmd_kill;
use commands::logs::{LineRange, cmd_logs, parse_head_spec, parse_tail_spec};
use commands::ls::cmd_ls;
use commands::prune::cmd_prune;
use commands::restart::cmd_restart;
use commands::run::run_script;
use presentation::{MARKER_BLUE, colored, error_label, stderr_color_enabled};
#[cfg(test)]
use std::fs;
use std::io::{self, Write, stdout};
use std::path::{Path, PathBuf};

/// Names `fog` handles itself instead of looking up as a script in `fog.json`.
const BUILTIN_COMMANDS: [&str; 7] = ["ls", "kill", "restart", "logs", "prune", "index", "attach"];

/// Whether `name` is a built-in command rather than a configured script.
fn is_builtin(name: &str) -> bool {
    BUILTIN_COMMANDS.contains(&name)
}

/// Command-line interface arguments parsed via clap.
#[derive(Parser)]
#[command(
    name = "fog",
    version = env!("CARGO_PKG_VERSION"),
    about = "Terminal-based service orchestrator & reverse-proxy dashboard",
    after_help = "Built-in commands:\n  fog ls [PID]                    list running instances and service status\n  fog kill [PID|--all]            gracefully shut down a running instance\n  fog kill --force [PID|--all]    forcibly shut down a wedged instance\n  fog restart [PID|--all]         restart a running instance\n  fog logs [PID]                  list services and their status\n  fog logs [PID] --service NAME   print captured output of one service\n  fog logs [PID] -s NAME --tail   limit lines: --head N|-N, --tail N|-N|+N\n  fog prune [--dry-run]           remove the captured logs of finished instances\n  fog attach [PID]                reattach the TUI to a running instance (replays screen history)\n  fog index serve [--foreground] [--port N]  run the index server (detached by default)\n  fog index kill                  stop the index server\n  fog index restart               restart the index server\n\nWith no PID, kill/restart/logs target the instance started from the local\nfog.json (same config directory, so the branch is implicit). `fog logs <PID>`\nalso reads the captured logs of an instance that has finished; `fog prune`\nremoves those logs (a running instance's logs are never touched). When several\nlocal instances match, the command lists them: pass an explicit PID, or\n--all (kill/restart) to apply to every local match. Outside a directory\nwith fog.json, --all applies to every running instance.\n\nWhen a kill/restart grace period is not enough (a wedged instance that never\nconsumes its kill flag), --force escalates to SIGTERM then SIGKILL.\n\nRun a script from fog.json:\n  fog <script> [OPTIONS]  (e.g. fog dev)\n\nWith no script, fog lists the scripts in fog.json, or prints this help when\nthere is no config in the current directory.\n\nOverride an allocated port for one run:\n  fog dev --port api=4000       (repeatable; --port web=0 re-randomizes)"
)]
struct Cli {
    /// Script to run (e.g. `fog dev`), or a built-in command
    /// (`ls`, `kill`, `restart`, `logs`, `attach`, `index`).
    script: Option<String>,

    /// PID of a running fog instance (used with `fog kill <pid>`, `fog restart <pid>`, `fog logs <pid>`).
    /// Omit it in a directory with `fog.json` to target the instance started
    /// from that config.
    pid: Option<u32>,

    /// Apply to every matching instance: the local `fog.json` matches when
    /// run in a directory with a config, otherwise every running instance.
    /// Only used with `fog kill` and `fog restart`; conflicts with `PID`.
    #[arg(long)]
    all: bool,

    /// Keep escalating when the instance does not stop gracefully: after the
    /// normal grace period, send SIGTERM and then SIGKILL to the process tree.
    /// Only used with `fog kill` and `fog restart`.
    #[arg(long)]
    force: bool,

    /// Also stop instances that were cold-started by the target through a
    /// cross-instance port reference. Only used with `fog kill`.
    #[arg(long)]
    with_dependents: bool,

    /// Only used with `fog logs`: print the captured output of this service
    /// instead of listing services. Without it, `fog logs` lists the
    /// available service names and their status.
    #[arg(short, long, value_name = "SERVICE")]
    service: Option<String>,

    /// Only used with `fog logs --service`: keep the first `N` lines, or all
    /// but the last `N` with `-N` (like `head -n`).
    #[arg(long, value_name = "N", allow_hyphen_values = true, value_parser = parse_head_spec)]
    head: Option<LineRange>,

    /// Only used with `fog logs --service`: keep the last `N` lines (or
    /// `-N`), or every line from `N` to the end with `+N` (like `tail -n`).
    #[arg(long, value_name = "N", allow_hyphen_values = true, value_parser = parse_tail_spec)]
    tail: Option<LineRange>,

    /// Path to the configuration file (or a directory containing `fog.json`).
    /// Defaults to `fog.json`.
    #[arg(short, long, default_value = "fog.json")]
    config: std::path::PathBuf,

    /// Save service output to `temp/<name>.txt` on exit.
    #[arg(long, help = "Save service output to temp/<name>.txt on exit")]
    save_logs: bool,

    /// Run the script in the git worktree checked out on this branch.
    #[arg(long)]
    branch: Option<String>,

    /// Print a shell completion script to stdout and exit.
    #[arg(long, value_name = "SHELL")]
    completions: Option<CompletionShell>,

    /// Run the script in the background without the TUI: services keep their
    /// PTYs, health checks and proxy, and their output is captured to
    /// `$TMPDIR/fog-<pid>.logs/` for inspection with `fog logs <pid>`.
    #[arg(short, long)]
    detach: bool,

    /// Ignore shared services: even if `share:true`/`reuse:true` with a passing
    /// `health_check`, start a fresh instance instead of borrowing/reusing.
    #[arg(long)]
    no_share: bool,

    /// Override a top-level `ports` entry for this run, e.g. `--port api=4000`.
    /// Repeatable; use `0` to re-randomize. Names not in the config `ports` map
    /// are added for this run. Only applies when running a script.
    #[arg(long, value_name = "NAME=PORT")]
    port: Vec<String>,

    /// Only used with `fog prune`: report what would be removed without
    /// deleting anything.
    #[arg(long)]
    dry_run: bool,

    /// Print verbose setup output (DNS, router, index, port and native-route
    /// details). Warnings are always printed.
    #[arg(short, long)]
    verbose: bool,
}

/// Resolves the config file to use, honoring `--branch`:
///
/// When `--branch <name>` is given, fog runs the script from the git worktree
/// checked out on that branch (a relative `--config` is resolved against the
/// worktree root). Errors out when no worktree has that branch.
fn resolve_run_config(cli: &Cli) -> PathBuf {
    let Some(branch) = &cli.branch else {
        return cli.config.clone();
    };

    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let worktrees = crate::worktree::list(&cwd).unwrap_or_else(|| {
        die(
            "--branch requires a git repository (could not list worktrees)",
            Some("run `fog --branch <name>` inside a git checkout"),
            1,
        )
    });

    let Some(wt) = worktrees
        .iter()
        .find(|w| w.branch.as_deref() == Some(branch.as_str()))
    else {
        eprintln!(
            "{} no worktree is checked out on branch '{}'",
            error_label(),
            branch
        );
        eprintln!("available worktrees:");
        for w in &worktrees {
            let label = w.branch.as_deref().unwrap_or("(detached)");
            eprintln!("  {:<24} {}", label, w.path.display());
        }
        std::process::exit(1);
    };

    eprintln!(
        "switching to worktree {} (branch {})",
        wt.path.display(),
        branch
    );
    if cli.config.is_absolute() {
        cli.config.clone()
    } else {
        wt.path.join(&cli.config)
    }
}

/// Resolves the config path: if `path` is a directory, looks for `fog.json`
/// inside it; otherwise returns the path unchanged.
fn resolve_config_path(path: &Path) -> PathBuf {
    if path.is_dir() {
        path.join("fog.json")
    } else {
        path.to_path_buf()
    }
}

/// Loads and parses the config file, exiting with a diagnostic on failure.
fn load_config(path: &Path) -> Config {
    if !path.exists() {
        die(
            &format!("no fog.json found at '{}'", path.display()),
            Some("create one here, pass --config <path>, or run `fog --help`"),
            1,
        );
    }
    let config = match crate::config::load(path) {
        Ok(c) => c,
        Err(e) => fail(e),
    };
    let issues = config.validate();
    if !issues.is_empty() {
        // One line per problem, path first, so a typo can be found by the same
        // dotted path the user would grep for.
        for issue in &issues {
            eprintln!("{} {issue}", error_label());
        }
        die(
            FogError::new(format!(
                "config '{}' has {} problem{}",
                path.display(),
                issues.len(),
                if issues.len() == 1 { "" } else { "s" }
            ))
            .with_hint(
                "fix the paths above; the config reference is https://naputt1.github.io/fog/configuration",
            ),
            None,
            1,
        );
    }
    config
}

/// The `  fog <name>` lines listing every script in `config`, sorted by name.
fn script_list_lines(config: &Config) -> Vec<String> {
    let mut names: Vec<&String> = config.scripts.keys().collect();
    names.sort();
    names
        .into_iter()
        .map(|name| format!("  fog {name}"))
        .collect()
}

/// Lists available script names and exits with an error.
fn list_scripts_and_exit(config: &Config, message: &str) -> ! {
    let tty = stderr_color_enabled();
    eprintln!("{} {message}", error_label());
    eprintln!();
    eprintln!("Available scripts:");
    for line in script_list_lines(config) {
        eprintln!("{line}");
    }
    eprintln!();
    eprintln!(
        "{}",
        colored(
            "hint: run `fog --help` for built-in commands",
            MARKER_BLUE,
            tty
        )
    );
    std::process::exit(2);
}

/// Prints `error: <message>` and an optional `hint:` line to stderr, then
/// exits with `code`.
fn die(error: impl Into<FogError>, hint: Option<&str>, code: i32) -> ! {
    let error = error.into();
    let tty = stderr_color_enabled();
    eprintln!("{} {}", error_label(), error.message());
    // An error that carries its own hint owns the recovery advice; the
    // explicit `hint` argument is for callers that only compute one.
    if let Some(hint) = error.hint().or(hint) {
        eprintln!("{} {hint}", colored("hint:", MARKER_BLUE, tty));
    }
    std::process::exit(code);
}

/// A runtime failure (the command was understood but could not run).
fn fail(error: impl Into<FogError>) -> ! {
    die(error, None, 1)
}

/// Bad or conflicting flags: exits with clap's usage exit code.
fn usage_error(message: impl Into<FogError>, hint: &str) -> ! {
    die(message, Some(hint), 2)
}

/// Prints the CLI help to stdout, tolerating a closed pipe (`fog | head`).
fn print_cli_help() -> io::Result<()> {
    let mut command = Cli::command();
    let result = command.print_help().and_then(|()| {
        let mut out = stdout();
        out.write_all(b"\n")?;
        out.flush()
    });
    match result {
        Err(err) if err.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        other => other,
    }
}

pub fn run() -> io::Result<()> {
    // `fog index serve/kill/restart` are dispatched before clap so they are not
    // misparsed as the `[PID]` positional (which expects a number).
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.first().map(String::as_str) == Some("index") {
        return commands::index::dispatch(&argv);
    }

    let matches = Cli::command().get_matches();
    // `config` has a default, so the only way to tell an explicit `--config`
    // (a missing file is an error) from the default (a missing file means
    // "print help") is where the value came from.
    let config_from_cli = matches.value_source("config") == Some(ValueSource::CommandLine);
    // `get_matches` already exited on a parse error, so this can only fail if
    // the derive and the command definition disagree.
    let cli = Cli::from_arg_matches(&matches).expect("clap arguments must deserialize");

    if let Some(shell) = cli.completions {
        print!("{}", crate::completion::generate(shell));
        return Ok(());
    }

    // `--service` only applies to `fog logs`; reject it elsewhere before the
    // detach path so it can never leak into a daemon child's arguments.
    if cli.service.is_some() && cli.script.as_deref() != Some("logs") {
        usage_error(
            "--service only applies to `fog logs`",
            "e.g. `fog logs --service api`",
        );
    }

    // `--head`/`--tail` only apply to `fog logs`, and only alongside a service
    // selection (the plain listing is already short).
    if cli.head.is_some() || cli.tail.is_some() {
        if cli.script.as_deref() != Some("logs") {
            usage_error(
                "--head/--tail only apply to `fog logs`",
                "e.g. `fog logs <pid> -s api --tail 50`",
            );
        }
        if cli.service.is_none() {
            usage_error(
                "--head/--tail require --service",
                "e.g. `fog logs <pid> -s api --tail 50`",
            );
        }
    }

    // `--all` only applies to `fog kill` / `fog restart`, and conflicts with
    // an explicit PID.
    if cli.all {
        match cli.script.as_deref() {
            Some("kill") | Some("restart") => {}
            Some("logs") => {
                usage_error(
                    "--all only applies to `fog kill` and `fog restart`",
                    "for logs, pass an explicit PID",
                );
            }
            _ => {
                usage_error(
                    "--all only applies to `fog kill` and `fog restart`",
                    "e.g. `fog kill --all`",
                );
            }
        }
        if cli.pid.is_some() {
            usage_error(
                "--all cannot be used with a PID",
                "drop the PID or drop --all",
            );
        }
    }

    // `--force` only applies to `fog kill` / `fog restart`.
    if cli.force && !matches!(cli.script.as_deref(), Some("kill") | Some("restart")) {
        usage_error(
            "--force only applies to `fog kill` and `fog restart`",
            "e.g. `fog kill --force`",
        );
    }

    // `fog prune` applies to every finished instance, so it takes no PID.
    if cli.script.as_deref() == Some("prune") && cli.pid.is_some() {
        usage_error(
            "`fog prune` takes no PID",
            "it removes the logs of every finished instance",
        );
    }

    // `--dry-run` only applies to `fog prune`.
    if cli.dry_run && cli.script.as_deref() != Some("prune") {
        usage_error(
            "--dry-run only applies to `fog prune`",
            "e.g. `fog prune --dry-run`",
        );
    }

    // `--port` only applies when running a script.
    if !cli.port.is_empty() && cli.script.as_deref().is_none_or(is_builtin) {
        usage_error(
            "--port only applies when running a script",
            "e.g. `fog dev --port api=4000`",
        );
    }

    // Detach: run the script in the background and return once it is serving.
    // The daemon child (re-executed with FOG_DAEMON_CHILD=1) takes the
    // headless path in run_script and must not re-daemonize.
    if cli.detach && std::env::var_os("FOG_DAEMON_CHILD").is_none() {
        match cli.script.as_deref() {
            Some(name) if !is_builtin(name) => {
                return daemonize(name);
            }
            Some(_) => {
                usage_error(
                    "--detach only applies to running a script",
                    "e.g. `fog dev -d`",
                );
            }
            None => {
                usage_error("--detach requires a script", "e.g. `fog dev -d`");
            }
        }
    }

    match cli.script.as_deref() {
        Some("ls") => cmd_ls(),
        Some("kill") => cmd_kill(cli.pid, cli.all, &cli),
        Some("restart") => cmd_restart(cli.pid, cli.all, &cli),
        Some("logs") => cmd_logs(cli.pid, cli.service.clone(), &cli),
        Some("prune") => cmd_prune(cli.dry_run, cli.verbose),
        Some("attach") => cmd_attach(&cli),
        Some(name) => run_script(name, &cli),
        None => {
            let config_path = resolve_config_path(&resolve_run_config(&cli));
            // `fog` on its own is an orientation prompt, not an error: show the
            // help when there is no config here, and the script list when there
            // is. Only an explicitly named missing config is an error.
            if !config_path.exists() && !config_from_cli {
                print_cli_help()?;
                return Ok(());
            }
            let config = load_config(&config_path);
            if config.scripts.is_empty() {
                fail(&format!(
                    "no scripts defined in '{}'",
                    config_path.display()
                ));
            }
            println!("Available scripts in '{}':", config_path.display());
            for line in script_list_lines(&config) {
                println!("{line}");
            }
            println!();
            println!("Run one with `fog <script>`; see `fog --help` for built-in commands.");
            Ok(())
        }
    }
}

/// Test-only scratch directory, shared by the command modules' tests.
#[cfg(test)]
pub(crate) fn temp_dir() -> PathBuf {
    // Many tests in this binary call `temp_dir` concurrently. The process
    // id is shared, and the clock reports two calls made in quick
    // succession as the same nanosecond, so a per-process counter is what
    // keeps the paths unique. Without it two tests can share a directory
    // and one test's cleanup deletes another's files mid-run.
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "fog-resolve-config-{}-{}-{}",
        std::process::id(),
        seq,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_resolve_config_path_directory() {
        let dir = temp_dir();
        fs::write(dir.join("fog.json"), "{}").unwrap();
        assert_eq!(resolve_config_path(&dir), dir.join("fog.json"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_resolve_config_path_file() {
        let dir = temp_dir();
        let file = dir.join("custom.json");
        fs::write(&file, "{}").unwrap();
        assert_eq!(resolve_config_path(&file), file);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_resolve_config_path_non_existent() {
        let dir = temp_dir();
        let missing = dir.join("missing.json");
        assert_eq!(resolve_config_path(&missing), missing);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_script_list_lines_sorts_and_indents_names() {
        let dir = temp_dir();
        let path = dir.join("fog.json");
        fs::write(&path, r#"{"scripts":{"web":{},"api":{}}}"#).unwrap();
        let config = crate::config::load(&path).unwrap();
        assert_eq!(script_list_lines(&config), vec!["  fog api", "  fog web"]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_cli_accepts_force_flag() {
        let cli = Cli::try_parse_from(["fog", "kill", "--force"]).unwrap();
        assert!(cli.force);
        assert_eq!(cli.script.as_deref(), Some("kill"));

        let cli = Cli::try_parse_from(["fog", "restart", "--all", "--force"]).unwrap();
        assert!(cli.force && cli.all);
    }

    #[cfg(unix)]
    #[test]
    fn test_cli_accepts_port_overrides() {
        let cli =
            Cli::try_parse_from(["fog", "dev", "--port", "api=4000", "--port=web=0"]).unwrap();
        assert_eq!(cli.port, vec!["api=4000".to_string(), "web=0".to_string()]);
        assert_eq!(cli.script.as_deref(), Some("dev"));
    }

    #[test]
    fn test_cli_accepts_logs_head_tail() {
        let cli = Cli::try_parse_from(["fog", "logs", "-s", "api", "--tail", "50"]).unwrap();
        assert_eq!(cli.tail, Some(LineRange::Last(50)));
        // A leading '-' must be accepted as the value, not parsed as a flag.
        let cli = Cli::try_parse_from(["fog", "logs", "-s", "api", "--head", "-50"]).unwrap();
        assert_eq!(cli.head, Some(LineRange::ExceptLast(50)));
        let cli = Cli::try_parse_from(["fog", "logs", "-s", "api", "--tail", "+51"]).unwrap();
        assert_eq!(cli.tail, Some(LineRange::From(51)));
    }
}
