use super::{App, Mode};
use crate::click_tab::ClickTab;
use crate::config_watcher;
use crate::ipc;
use crate::runtime;
use crate::worktree;
use crate::worktree::Worktree;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

/// An open worktree-switch popup: the repository's worktrees plus an
/// incremental fuzzy filter, a selected row, live-branch markers, and a
/// transient status line. `f`-search mode feeds the filter (Esc returns to
/// browsing); `d` terminates the selected branch's live instances.
pub(crate) struct SwitchPopup {
    pub(crate) worktrees: Vec<Worktree>,
    pub(crate) filter: String,
    pub(crate) selected: usize,
    pub(crate) searching: bool,
    /// Branches that currently have a live fog instance serving them,
    /// rendered with a green asterisk.
    pub(crate) running: Vec<String>,
    /// Transient status message (e.g. the terminate outcome), cleared by the
    /// next key press.
    pub(crate) status: Option<String>,
}

impl SwitchPopup {
    /// The worktrees matching the current filter, in original order.
    pub(crate) fn matches(&self) -> Vec<Worktree> {
        if self.filter.is_empty() {
            return self.worktrees.clone();
        }
        self.worktrees
            .iter()
            .filter(|w| {
                subsequence_match(&w.label(), &self.filter)
                    || subsequence_match(&w.path.to_string_lossy(), &self.filter)
            })
            .cloned()
            .collect()
    }
}

/// Case-insensitive subsequence test: every char of `needle` appears in
/// `haystack` in order, not necessarily contiguously.
pub(crate) fn subsequence_match(haystack: &str, needle: &str) -> bool {
    let mut needle = needle.chars().flat_map(char::to_lowercase);
    let mut expected = needle.next();
    for c in haystack.chars().flat_map(char::to_lowercase) {
        let Some(exp) = expected else { return true };
        if c == exp {
            expected = needle.next();
        }
    }
    expected.is_none()
}

impl App {
    /// Opens the worktree-switch popup, listing the repository's worktrees.
    pub(crate) fn open_switch_popup(&mut self) {
        let config_dir = self
            .config_path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        match worktree::list(&config_dir) {
            Some(worktrees) if !worktrees.is_empty() => {
                self.switch_popup = Some(SwitchPopup {
                    running: self.running_branches(),
                    worktrees,
                    filter: String::new(),
                    selected: 0,
                    searching: false,
                    status: None,
                });
            }
            _ => {
                self.note_error("no git worktrees found in this repository".to_string());
            }
        }
    }

    /// Branches with a live fog instance serving this project's script,
    /// discovered by scanning the project's IPC sockets. The instance
    /// scanning already excludes this process, so the current branch is only
    /// listed when a second instance serves it.
    pub(crate) fn running_branches(&self) -> Vec<String> {
        let Some(project) = self.ipc_state.project.as_deref() else {
            return Vec::new();
        };
        let mut running = Vec::new();
        for (_, _, status) in ipc::find_instances_any_branch(project, &self.ipc_state.script) {
            if let Some(branch) = status.branch
                && !running.contains(&branch)
            {
                running.push(branch);
            }
        }
        running
    }

    /// Terminates every live fog instance serving the selected worktree's
    /// branch (via IPC kill requests with a SIGTERM fallback) and reports the
    /// outcome in the popup's transient status line. This instance is never
    /// a target: the instance scan excludes the current process, so `d` on
    /// the current branch only kills *other* instances sharing that branch.
    pub(crate) fn terminate_selected_branch(&mut self) {
        let (branch, is_current) = {
            let Some(popup) = &self.switch_popup else {
                return;
            };
            let matches = popup.matches();
            if matches.is_empty() {
                return;
            }
            let wt = &matches[popup.selected.min(matches.len() - 1)];
            let config_dir = self.config_path.parent().unwrap_or_else(|| Path::new("."));
            let is_current = worktree::is_current_worktree(wt, &popup.worktrees, config_dir);
            (wt.branch.clone(), is_current)
        };
        // Detached worktrees have `branch == None`. They are still killable
        // by matching `branch == None` instances (legacy/detached runs), so
        // we don't early-return here — we let the `find_*` scan decide.
        let project = self.ipc_state.project.clone();
        let script = self.ipc_state.script.clone();
        let instances = match project {
            Some(project) => ipc::find_instances_with_status(&project, &script, branch.as_deref())
                .into_iter()
                .map(|(pid, path, _)| (pid, path))
                .collect::<Vec<_>>(),
            None => Vec::new(),
        };
        let terminated = ipc::terminate_instances(&instances);
        if let Some(popup) = &mut self.switch_popup {
            popup.status = Some(match terminated {
                0 if branch.is_none() => {
                    "no running instances on this branch (detached)".to_string()
                }
                0 if is_current => "no other instances on this branch".to_string(),
                0 => "no running instances on this branch".to_string(),
                1 => "terminated 1 instance".to_string(),
                n => format!("terminated {n} instances"),
            });
        }
    }

    /// Switches this instance to run the given worktree's script in place:
    /// shared (reuse) services are handed over internally, non-reuse services
    /// are torn down, and tabs/proxy/config-watcher are rebuilt from the
    /// target worktree's config file.
    ///
    /// Returns `Ok(())` on success (popup should be closed) or `Err(msg)` on
    /// failure (caller should keep the popup open and show `msg` as status).
    pub(crate) fn switch_worktree(&mut self, wt: &Worktree) -> Result<(), String> {
        // Guard against switching to the worktree we are already on.
        let current_dir = self.config_path.parent().unwrap_or_else(|| Path::new("."));
        let already_here = if let Some(list) = worktree::list(current_dir) {
            worktree::is_current_worktree(wt, &list, current_dir)
        } else {
            wt.contains(current_dir)
        };
        if already_here {
            return Err("already on this branch".to_string());
        }
        // Resolve and validate the target worktree's config first, so a failure
        // leaves the current instance untouched.
        //
        // When `--config` was an absolute path that lives inside the current
        // worktree (the `cargo run -- --config /…/worktree/<id>/ui` case), we
        // rebase it onto the target worktree instead of sharing the same file.
        // Absolute paths outside any worktree (e.g. `~/.config/fog/fog.json`)
        // stay shared, per user preference.
        let config_path = if self.config_rel.is_absolute() {
            if let Some(list) = worktree::list(current_dir) {
                let cur_root = worktree::containing_worktree(&list, &self.config_path)
                    .or_else(|| worktree::containing_worktree(&list, &self.config_rel));
                if let Some(cur_root) = cur_root {
                    let cur_can = cur_root
                        .path
                        .canonicalize()
                        .unwrap_or_else(|_| cur_root.path.clone());
                    let cfg_can = self
                        .config_path
                        .canonicalize()
                        .unwrap_or_else(|_| self.config_path.clone());
                    let rel_can = self
                        .config_rel
                        .canonicalize()
                        .unwrap_or_else(|_| self.config_rel.clone());
                    // Try canonical strip first, then raw strip for non-existent paths
                    let rel = cfg_can
                        .strip_prefix(&cur_can)
                        .or_else(|_| rel_can.strip_prefix(&cur_can))
                        .ok()
                        .or_else(|| {
                            self.config_path
                                .strip_prefix(&cur_root.path)
                                .or_else(|_| self.config_rel.strip_prefix(&cur_root.path))
                                .ok()
                        });
                    if let Some(rel) = rel {
                        wt.path.join(rel)
                    } else {
                        // Inside repo but strip failed (e.g. symlinks) — fall back to share
                        self.config_rel.clone()
                    }
                } else {
                    // Absolute outside any worktree -> share
                    self.config_rel.clone()
                }
            } else {
                self.config_rel.clone()
            }
        } else {
            wt.path.join(&self.config_rel)
        };
        let config =
            crate::config::load(&config_path).map_err(|e| format!("switch worktree: {e}"))?;
        let script_name = self.ipc_state.script.clone();
        let Some(script) = config.scripts.get(&script_name) else {
            return Err(format!(
                "switch worktree: script '{}' not found in '{}'",
                script_name,
                config_path.display()
            ));
        };
        // Validate the dependency graph up front so a bad target script leaves
        // the current instance untouched instead of tearing it down first.
        let entries = script.service.clone().unwrap_or_default();
        if let Err(e) = runtime::resolve_dep_order(&entries) {
            return Err(format!("switch worktree: {e}"));
        }
        let config_path = config_path.canonicalize().unwrap_or(config_path);
        let config_dir = config_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();

        // Allocate ports and validate before tearing down the current instance,
        // so a failure leaves the running services untouched.
        let branch_for_ports = runtime::resolve_branch(&config_dir);
        let port_map = if let Some(specs) = &config.ports {
            crate::ports::allocate_ports(specs)
                .map_err(|e| format!("switch worktree: port allocation: {e}"))?
        } else {
            std::collections::HashMap::new()
        };
        if let Err(e) = crate::ports::ensure_ports_defined(
            config.ports.as_ref(),
            script,
            config.native_routes.as_ref(),
        ) {
            return Err(format!("switch worktree: {e}"));
        }
        // Adopt the live ports of a sibling that already owns a shared service
        // on the target branch, so its dependents resolve to the real port.
        let mut port_map = port_map;
        let owned_shared = runtime::adopt_shared_ports(
            script,
            &script_name,
            &config_dir,
            self.ipc_state.project.as_deref(),
            branch_for_ports.as_deref(),
            &mut port_map,
            self.no_share,
        );

        // Build the new runtime *before* tearing down the old one, so a
        // build failure doesn't leave the UI empty.
        let mut adopted: HashMap<String, ipc::HandoffItem> = HashMap::new();
        if !self.no_share {
            for item in &mut self.items {
                if item.reused || item.shared {
                    if let Some(handoff) = item.extract_handoff() {
                        adopted.insert(handoff.name.clone(), handoff);
                    } else {
                        item.preserve_for_reuse();
                    }
                }
            }
        }
        let title_branch = branch_for_ports.clone();
        let built = match runtime::build_with_ports_no_share(
            script,
            &script_name,
            &config_dir,
            self.ipc_state.project.clone(),
            self.save_logs,
            self.scrollback,
            None,
            &mut adopted,
            &port_map,
            branch_for_ports.clone(),
            self.no_share,
            &owned_shared,
        ) {
            Ok(b) => b,
            Err(e) => {
                // Close any fds we duped for handoff that didn't get consumed
                for (_, handoff) in adopted.drain() {
                    crate::fds::close(handoff.fd);
                }
                return Err(format!("switch worktree: {e}"));
            }
        };

        // Surface any non-fatal config warnings (e.g. share without health
        // check) raised while building the new runtime.
        for warning in &built.warnings {
            self.note_error(warning.clone());
        }

        // The project identity is the repo's git-common-dir, shared by every
        // worktree, so `ipc_state.project` stays unchanged across switches.
        self.title_branch = title_branch;

        // Tear down the old services and proxy now so their ports are free
        // before the new worktree's services start.
        self.proxy = None;
        self.items.clear();
        self.pending_services.clear();
        self.tabs = ClickTab::new(Vec::new(), self.sidebar_min, self.sidebar_max);
        self.proxy_tab_index = None;
        // Publish new ports/routes to IPC before ensuring Traefik, so the index UI
        // can synthesize native entries for the Services page.
        {
            *self.ipc_state.ports.lock().expect("mutex poisoned") = port_map.clone();
            let mut routes: Vec<crate::ipc::NativeRouteInfo> = config
                .native_routes
                .clone()
                .unwrap_or_default()
                .into_iter()
                .map(|r| crate::ipc::NativeRouteInfo {
                    host: r.host,
                    service: r.service,
                    port: r.port,
                    path_prefix: r.path_prefix,
                    endpoint: r.endpoint,
                })
                .collect();
            // Declared endpoint routes, flattened from the freshly built
            // terminals (resolved against the new ports/branch).
            routes.extend(runtime::endpoint_route_infos(&built.items));
            *self.ipc_state.native_routes.lock().expect("mutex poisoned") = routes;
        }
        // Clean up stale native routes from the previous branch before ensuring the new ones
        let prev_branch = runtime::resolve_branch(
            self.config_path
                .parent()
                .unwrap_or_else(|| std::path::Path::new(".")),
        );
        if prev_branch != branch_for_ports {
            crate::router::cleanup_native_routes(prev_branch.as_deref(), &config);
        }
        if let Some(routes) = &config.native_routes {
            for msg in crate::router::ensure_native_routes(
                routes,
                &port_map,
                branch_for_ports.as_deref(),
                &config,
                self.verbose,
            ) {
                self.note_error(msg);
            }
        }
        // Declared endpoint routes for the new branch.
        for msg in crate::router::ensure_native_routes(
            &runtime::endpoint_routes(&built.items),
            &port_map,
            branch_for_ports.as_deref(),
            &config,
            self.verbose,
        ) {
            self.note_error(msg);
        }

        let (tabs, proxy_tab_index) = Self::build_tabs(
            &built.items,
            &built.pending_services,
            built.proxy.is_some(),
            self.sidebar_min,
            self.sidebar_max,
        );

        self.items = built.items;
        self.pending_services = built.pending_services;
        self.proxy = built.proxy;
        self.tabs = tabs;
        self.proxy_tab_index = proxy_tab_index;
        self.config_path = config_path;
        // Stop the old watcher (it observes the flag within ~100ms) and replace
        // it with one owning the new stop flag we store.
        self.config_watcher_stop.store(true, Ordering::SeqCst);
        let (config_rx, config_watcher_stop) =
            config_watcher::spawn_config_watcher(self.config_path.clone());
        self.config_rx = config_rx;
        self.config_watcher_stop = config_watcher_stop;

        self.tabs.index = 0;
        self.scroll_offset = 0;
        self.mode = Mode::Normal;
        self.proxy_filter.clear();
        self.show_help = false;
        self.switch_popup = None;
        self.selecting = false;
        self.select_start = None;
        self.select_end = None;
        self.scrollbar_dragging = false;
        self.auto_scrolling = None;
        Ok(())
    }
}
