use super::{App, Mode};
use crate::click_tab::ClickTab;
use crate::config_watcher;
use crate::error::{FogError, FogResult};
use crate::ipc;
use crate::runtime;
use crate::worktree;
use crate::worktree::Worktree;
use std::collections::HashMap;
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
        let config_dir = self.project_dir();
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
            let config_dir = self.project_dir();
            let is_current = worktree::is_current_worktree(wt, &popup.worktrees, &config_dir);
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
    pub(crate) fn switch_worktree(&mut self, wt: &Worktree) -> FogResult<()> {
        // Guard against switching to the worktree we are already on.
        let current_dir = self.project_dir();
        let already_here = if let Some(list) = worktree::list(&current_dir) {
            worktree::is_current_worktree(wt, &list, &current_dir)
        } else {
            wt.contains(&current_dir)
        };
        if already_here {
            return Err(FogError::new("already on this branch"));
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
            if let Some(list) = worktree::list(&current_dir) {
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
        let config = crate::config::load(&config_path)
            .map_err(|e| FogError::new(format!("switch worktree: {e}")))?;
        let script_name = self.ipc_state.script.clone();
        let Some(script) = config.scripts.get(&script_name) else {
            return Err(FogError::new(format!(
                "switch worktree: script '{}' not found in '{}'",
                script_name,
                config_path.display()
            )));
        };
        // Replay the *currently* skipped services (not the startup set): one
        // started manually during this run must not become skipped again in
        // the target worktree. A service the target has since removed is
        // ignored rather than aborting the switch.
        let current: Vec<String> = self
            .items
            .iter()
            .filter(|item| item.skipped)
            .map(|item| item.name.clone())
            .collect();
        // Validate the dependency graph up front so a bad target script leaves
        // the current instance untouched instead of tearing it down first.
        let entries = script.service.clone().unwrap_or_default();
        if let Err(e) = runtime::resolve_dep_order(&entries) {
            return Err(FogError::new(format!("switch worktree: {e}")));
        }
        // Same logical-path rule as `run`: the worktree containing the config
        // link is the project root, not the symlink target's directory. With a
        // `--project-dir` override the target worktree becomes the new root,
        // since the config file itself lives outside every worktree.
        let config_path = std::path::absolute(&config_path).unwrap_or(config_path);
        let adopt_project_dir = self.project_dir.is_some().then(|| wt.path.clone());
        let config_dir = adopt_project_dir
            .clone()
            .unwrap_or_else(|| crate::project::config_dir(&config_path));
        let without = runtime::filter_without(script, &config_dir, &current);
        // Carry the current forced-share set into the target, dropping names it
        // no longer defines. A stale name would otherwise reach a detach
        // successor or `fog attach` and be rejected there.
        let forced: Vec<String> = self.share.forced.iter().cloned().collect();
        let mut forced = runtime::filter_forced_share(script, &config_dir, &forced);
        forced.retain(|name| !without.contains(name));
        let share = runtime::SharePolicy {
            no_share: self.share.no_share,
            forced,
        };
        // Ports of services this instance owns and is about to hand over: the
        // adopted process keeps listening on them, so dependent templates must
        // resolve to the old values rather than a fresh allocation.
        // Only services the target still shares can consume a handoff. A name
        // whose target entry dropped `share`/`reuse` must keep a fresh port and
        // let the old process be torn down, otherwise the new copy is spawned
        // on a port still held by the old one.
        let target_ports = runtime::shared_port_names(script, &config_dir, &share);
        let target_shared: std::collections::HashSet<String> =
            target_ports.keys().cloned().collect();
        let mut handoff_ports = std::collections::HashMap::new();
        if !share.no_share {
            let current_ports = self.ipc_state.ports.lock().expect("mutex poisoned").clone();
            for item in &self.items {
                if item.can_handoff()
                    && target_shared.contains(&item.name)
                    && let Some(names) = target_ports.get(&item.name)
                {
                    for name in names {
                        if let Some(port) = current_ports.get(name) {
                            handoff_ports.insert(name.clone(), *port);
                        }
                    }
                }
            }
        }
        // Allocate ports and validate before tearing down the current instance,
        // so a failure leaves the running services untouched.
        let branch_for_ports = runtime::resolve_branch(&config_dir);
        if let Err(e) = crate::ports::ensure_ports_defined(
            config.ports.as_ref(),
            script,
            config.native_routes.as_ref(),
        ) {
            return Err(FogError::new(format!("switch worktree: {e}")));
        }
        // Adopt the live ports of a sibling that already owns a shared service
        // on the target branch, so its dependents resolve to the real port.
        // Adoption happens *before* allocation: a fixed port held by the live
        // owner would otherwise fail its "already in use" probe.
        let mut adopted_ports = std::collections::HashMap::new();
        let owned_shared = runtime::adopt_shared_ports(
            script,
            &script_name,
            &config_dir,
            self.ipc_state.project.as_deref(),
            branch_for_ports.as_deref(),
            &mut adopted_ports,
            &share,
            &without,
        );
        let specs = config.ports.clone().unwrap_or_default();
        let unprobed =
            runtime::unprobed_port_names(script, &config_dir, &share, &without, Some(&specs));
        let local_specs: std::collections::HashMap<String, u16> = specs
            .into_iter()
            .filter(|(name, _)| {
                !adopted_ports.contains_key(name) && !handoff_ports.contains_key(name)
            })
            .collect();
        let mut port_map = if local_specs.is_empty() {
            std::collections::HashMap::new()
        } else {
            crate::ports::allocate_ports_unprobed(&local_specs, &unprobed)
                .map_err(|e| FogError::new(format!("switch worktree: port allocation: {e}")))?
        };
        port_map.extend(adopted_ports);
        port_map.extend(handoff_ports);
        // Resolve `${ports.<name>@<repo>:<branch>[:<script>]}` against live
        // instances (cold-starting a `:script` target when needed) before
        // building, so a missing dependency aborts the switch while the
        // current instance is still running.
        if let Err(e) = crate::references::resolve(
            script,
            config.native_routes.as_ref(),
            crate::references::Owner {
                config_dir: &config_dir,
                project: self.ipc_state.project.as_deref(),
                branch: branch_for_ports.as_deref(),
                script_name: &script_name,
            },
            &mut port_map,
            &mut Vec::new(),
        ) {
            return Err(FogError::new(format!("switch worktree: {e}")));
        }

        // Build the new runtime *before* tearing down the old one, so a build
        // failure doesn't leave the UI empty. A fixed port still held by the
        // old services can fail allocation here; that is safer than tearing
        // them down first and then discovering the target cannot start.
        let mut adopted: HashMap<String, ipc::HandoffItem> = HashMap::new();
        if !self.share.no_share {
            for item in &mut self.items {
                if item.can_handoff() && target_shared.contains(&item.name) {
                    if let Some(handoff) = item.extract_handoff() {
                        adopted.insert(handoff.name.clone(), handoff);
                    } else {
                        item.preserve_for_reuse();
                    }
                }
            }
        }
        let title_branch = branch_for_ports.clone();
        let built = match runtime::build_with_share_policy(
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
            share.clone(),
            false,
            &owned_shared,
            &without,
        ) {
            Ok(b) => b,
            Err(e) => {
                // Close any fds we duped for handoff that didn't get consumed
                for (_, handoff) in adopted.drain() {
                    crate::fds::close(handoff.fd);
                }
                return Err(FogError::new(format!("switch worktree: {e}")));
            }
        };

        // Surface any non-fatal config warnings (e.g. share without health
        // check) raised while building the new runtime.
        for warning in &built.warnings {
            self.note_error(warning.clone());
        }
        self.share = share.clone();
        if let Some(dir) = adopt_project_dir {
            self.project_dir = Some(dir);
        }
        *self.ipc_state.forced_share.lock().expect("mutex poisoned") =
            share.forced.iter().cloned().collect();

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
        let prev_branch = runtime::resolve_branch(&current_dir);
        if prev_branch != branch_for_ports {
            crate::router::cleanup_native_routes(prev_branch.as_deref(), &current_dir, &config);
        }
        if let Some(routes) = &config.native_routes {
            for msg in crate::router::ensure_native_routes(
                routes,
                &port_map,
                branch_for_ports.as_deref(),
                &config_dir,
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
            &config_dir,
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
