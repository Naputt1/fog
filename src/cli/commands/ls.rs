//! `fog ls`: every running instance with its identity columns and a per-service
//! status sub-table.
//!
//! A socket that no longer answers is only deleted when its process is gone: a
//! live-but-slow instance (mid-handoff, say) must not be hidden by removing
//! state another process still owns.

use crate::cli::presentation::{color_enabled, colored, service_table, state_color};
use crate::ipc;
use crate::service_state::ServiceState;
use std::fs;
use std::io;

/// A service line in the `fog ls` sub-table: (service name, state).
type ServiceEntry = (String, String);

/// One running instance: identity columns plus the services sub-table.
pub(crate) struct InstanceRow {
    pid: u32,
    script: String,
    project: String,
    branch: String,
    started_by: String,
    proxy: String,
    services: Vec<ServiceEntry>,
}

pub(crate) fn cmd_ls() -> io::Result<()> {
    let instances = ipc::find_instances()?;

    if instances.is_empty() {
        println!("no running fog instances");
        return Ok(());
    }

    let mut rows: Vec<InstanceRow> = Vec::new();
    for (pid, path) in &instances {
        match ipc::query_status(path) {
            Ok(status) => {
                let proxy = match status.proxy {
                    Some(p) if p.running => format!(":{}", p.port),
                    Some(_) => ":down".to_string(),
                    None => "-".to_string(),
                };
                let services = status
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
                    .collect::<Vec<_>>();
                let project = status
                    .project
                    .map(|p| crate::project::display_name(&p))
                    .unwrap_or_else(|| "-".to_string());
                let branch = status.branch.unwrap_or_else(|| "-".to_string());
                let started_by = match (status.autostarted_by, status.autostarted_by_label) {
                    (Some(parent), Some(label)) => format!("{label} ({parent})"),
                    (Some(parent), None) => parent.to_string(),
                    (None, _) => "-".to_string(),
                };
                rows.push(InstanceRow {
                    pid: *pid,
                    script: status.script,
                    project,
                    branch,
                    started_by,
                    proxy,
                    services,
                });
            }
            Err(_) => {
                // Only treat the socket as stale if the owning process is
                // genuinely gone. A live-but-slow instance (e.g. mid-handoff)
                // must not be hidden by deleting its socket.
                if !crate::process::is_pid_alive(*pid) {
                    let _ = fs::remove_file(path);
                }
            }
        }
    }

    if rows.is_empty() {
        println!("no running fog instances");
        return Ok(());
    }

    let max = |len: usize, header: &str| len.max(header.len());
    let w_pid = rows
        .iter()
        .map(|r| r.pid.to_string().len())
        .max()
        .unwrap_or(0);
    let w_script = rows.iter().map(|r| r.script.len()).max().unwrap_or(0);
    let w_project = rows.iter().map(|r| r.project.len()).max().unwrap_or(0);
    let w_branch = rows.iter().map(|r| r.branch.len()).max().unwrap_or(0);
    let w_started_by = rows.iter().map(|r| r.started_by.len()).max().unwrap_or(0);

    let w_pid = max(w_pid, "pid");
    let w_script = max(w_script, "script");
    let w_project = max(w_project, "project");
    let w_branch = max(w_branch, "branch");
    let w_started_by = max(w_started_by, "started-by");

    println!(
        "{:>w_pid$}  {:<w_script$}  {:<w_project$}  {:<w_branch$}  {:<w_started_by$}  proxy",
        "pid", "script", "project", "branch", "started-by"
    );
    let color = color_enabled();
    for row in rows {
        let proxy = match state_color(&row.proxy) {
            Some(ansi) => colored(&row.proxy, ansi, color),
            None => row.proxy.clone(),
        };
        println!(
            "{:>w_pid$}  {:<w_script$}  {:<w_project$}  {:<w_branch$}  {:<w_started_by$}  {proxy}",
            row.pid, row.script, row.project, row.branch, row.started_by
        );
        if !row.services.is_empty() {
            for line in service_table(&row.services, "  ", color) {
                println!("{line}");
            }
        }
        println!();
    }
    Ok(())
}
