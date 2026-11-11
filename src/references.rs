//! Cross-instance port references:
//! `${ports.<name>@<repo>:<branch>[:<script>]}`.
//!
//! The value always comes from a running fog instance's published port map.
//! With the optional `:script` segment, fog may start that instance when it is
//! not running: the project is located through the project registry, the
//! branch through `git worktree list`, and the target `fog.json` in that
//! worktree must declare the referenced port. Values are captured once, at
//! startup, and published like allocated ports.

use crate::config::{NativeRouteConfig, ScriptConfig};
use crate::ports::{self, ExternalPortRef, PickFailure, PortMap};
use crate::worktree::{self, Worktree};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Environment variable carrying the chain of instances started by
/// cross-instance references, so a cycle can be reported instead of looping.
pub const AUTOSTART_CHAIN_ENV: &str = "FOG_AUTOSTART_CHAIN";

/// PID of the fog instance that cold-started this one (set by
/// [`crate::ipc::spawn_detached`]'s caller in [`resolve`]).
pub const AUTOSTART_BY_ENV: &str = "FOG_AUTOSTART_BY";

/// Human-readable identity (`repo:branch:script`) of that parent.
pub const AUTOSTART_PARENT_ENV: &str = "FOG_AUTOSTART_PARENT";

/// How long a cold-started instance may take to publish its IPC status.
const AUTOSTART_READY_TIMEOUT: Duration = Duration::from_secs(60);

/// Field separator inside one serialized chain entry (unit separator).
const CHAIN_FIELD: char = '\u{1f}';

/// The instance whose references are being resolved.
#[derive(Debug, Clone, Copy)]
pub struct Owner<'a> {
    pub config_dir: &'a Path,
    pub project: Option<&'a str>,
    pub branch: Option<&'a str>,
    pub script_name: &'a str,
}

/// Resolves every cross-instance reference in `script` and its native routes.
///
/// References already present in `ports` (an explicit `--port` override or a
/// takeover) win and are left untouched. Live instances are consulted first;
/// only references that name a script and find no matching instance may start
/// one. `messages` receives one line per started instance for the startup log.
pub fn resolve(
    script: &ScriptConfig,
    native_routes: Option<&Vec<NativeRouteConfig>>,
    owner: Owner<'_>,
    ports_map: &mut PortMap,
    messages: &mut Vec<String>,
) -> Result<(), String> {
    let references = ports::collect_external_port_refs(script, native_routes)?;
    if references.is_empty() {
        return Ok(());
    }

    let instances = ports::live_instances();
    let mut pending: Vec<Request> = Vec::new();
    for reference in references {
        let key = reference.key();
        if ports_map.contains_key(&key) {
            continue;
        }
        let request = request_for(reference, owner)?;
        // `--port` may pin the expanded branch (`api@repo:dev`) even when the
        // config uses `${branch}`; apply it under the template's own key.
        if let Some(port) = ports_map.get(&request.matcher.key()).copied() {
            ports_map.insert(key, port);
            continue;
        }
        match ports::pick_external_port_source(&request.matcher, &instances) {
            Ok((port, pid)) => {
                ports_map.insert(key, port);
                messages.push(resolved_message(&request, port, pid));
            }
            Err(PickFailure::NoInstance) => pending.push(request),
            Err(failure) => {
                return Err(ports::format_pick_failure(&request.original, &failure));
            }
        }
    }
    if pending.is_empty() {
        return Ok(());
    }

    // Fail fast, before starting anything, when a pending reference cannot be
    // started: only the `:script` form authorizes a cold start.
    if let Some(request) = pending
        .iter()
        .find(|request| request.original.script.is_none())
    {
        return Err(ports::format_pick_failure(
            &request.original,
            &PickFailure::NoInstance,
        ));
    }

    let own = own_identity(owner);
    let parent_label = own.label();
    let parent_pid = std::process::id().to_string();
    let mut inherited = StartChain::from_env();
    inherited.push(own);

    // Resolve each distinct target once, so several references to the same
    // instance start it a single time.
    let mut groups: BTreeMap<ChainEntry, (Target, Vec<Request>)> = BTreeMap::new();
    for request in &pending {
        let target = Target::resolve(&request.matcher)?;
        groups
            .entry(target.identity())
            .or_insert_with(|| (target, Vec::new()))
            .1
            .push(request.clone());
    }

    for (entry, (target, group)) in &groups {
        if inherited.contains(entry) {
            return Err(cycle_error(group, &inherited, target));
        }
    }

    for (entry, (target, group)) in &groups {
        let start = crate::lock::with_file_lock(&start_lock_path(entry), || {
            // Another fog may have started it while this process waited for the
            // lock; then there is nothing left to do.
            if ports::any_instance_matches(&group[0].matcher, &ports::live_instances()) {
                return Ok(None);
            }
            crate::ipc::spawn_detached(
                &target.config_path,
                &target.script,
                None,
                &[
                    (AUTOSTART_CHAIN_ENV, inherited.serialize()),
                    (AUTOSTART_BY_ENV, parent_pid.clone()),
                    (AUTOSTART_PARENT_ENV, parent_label.clone()),
                ],
                AUTOSTART_READY_TIMEOUT,
            )
            .map(Some)
            .map_err(|e| e.to_string())
        });
        match start {
            Ok(Some(pid)) => messages.push(format!("started {} (pid {pid})", target.label())),
            Ok(None) => {}
            Err(e) => {
                return Err(format!(
                    "'${{{}}}' needs '{}', which could not be started automatically: {e}",
                    group[0].original.atom(),
                    target.label()
                ));
            }
        }
    }

    // The instances are ready enough to publish their port maps; resolve every
    // pending reference against the fresh snapshot.
    let instances = ports::live_instances();
    for request in &pending {
        let (port, pid) = ports::pick_external_port_source(&request.matcher, &instances)
            .map_err(|failure| ports::format_pick_failure(&request.original, &failure))?;
        ports_map.insert(request.original.key(), port);
        messages.push(resolved_message(request, port, pid));
    }
    Ok(())
}

/// A reference as written, plus the concrete branch token used to match live
/// instances and worktrees.
#[derive(Debug, Clone)]
struct Request {
    original: ExternalPortRef,
    matcher: ExternalPortRef,
    /// Extra wording for the resolved-value log line, e.g. a `?` fallback.
    note: Option<String>,
}

fn request_for(reference: ExternalPortRef, owner: Owner<'_>) -> Result<Request, String> {
    let (matcher, note) = concrete_branch(&reference, owner)?;
    Ok(Request {
        original: reference,
        matcher,
        note,
    })
}

/// `resolved '<atom>' = <port> (pid <pid>[, <note>])`.
fn resolved_message(request: &Request, port: u16, pid: u32) -> String {
    let note = request
        .note
        .as_deref()
        .map(|note| format!(", {note}"))
        .unwrap_or_default();
    format!(
        "resolved '{}' = {port} (pid {pid}{note})",
        request.original.atom()
    )
}

/// Resolves `${branch}`-style atoms and the `?` / `*` branch tokens in a
/// reference's branch, returning the matcher plus a note for the log.
fn concrete_branch(
    reference: &ExternalPortRef,
    owner: Owner<'_>,
) -> Result<(ExternalPortRef, Option<String>), String> {
    let mut matcher = reference.clone();
    match reference.branch.as_str() {
        "*" => {
            matcher.branch = main_branch_of(&reference.project).map_err(|e| {
                format!(
                    "'${{{}}}': could not resolve '*' to a branch: {e}",
                    reference.atom()
                )
            })?;
            Ok((matcher, None))
        }
        "?" => {
            let (branch, fell_back) = same_branch_or_main(&reference.project, owner.branch)
                .map_err(|e| {
                    format!(
                        "'${{{}}}': could not resolve '?' to a branch: {e}",
                        reference.atom()
                    )
                })?;
            matcher.branch = branch;
            let note = fell_back.then(|| format!("fell back to '{}'", matcher.branch));
            Ok((matcher, note))
        }
        _ => {
            let Some(branch) = ports::expand_branch_token(&reference.branch, owner.branch) else {
                return Err(format!(
                    "'${{{}}}' uses '${{branch}}' but this config has no branch (detached HEAD); \
                     use a concrete branch, '?' for the same branch with the other repo's main \
                     worktree as fallback, '*' for its main worktree, or --port",
                    reference.atom()
                ));
            };
            matcher.branch = branch;
            Ok((matcher, None))
        }
    }
}

/// The branch checked out in the target repo's main worktree, for `*`.
fn main_branch_of(project: &str) -> Result<String, String> {
    let repo = find_repo(project)?;
    let worktrees = target_worktrees(&repo)?;
    // `git worktree list` prints the main worktree first.
    let main = worktrees
        .first()
        .ok_or_else(|| format!("'{}' has no worktrees", repo.name))?;
    main.branch.clone().ok_or_else(|| {
        format!(
            "the main worktree of '{}' is on a detached HEAD; use a concrete branch",
            repo.name
        )
    })
}

/// The branch for `?`: the target worktree on the referencing branch when the
/// repo has one, otherwise its main-worktree branch. The flag says which.
fn same_branch_or_main(
    project: &str,
    owner_branch: Option<&str>,
) -> Result<(String, bool), String> {
    let repo = find_repo(project)?;
    let worktrees = target_worktrees(&repo)?;
    if let Some(owner) = owner_branch
        && let Some(branch) = pick_branch(&worktrees, owner)?
    {
        return Ok((branch, false));
    }
    Ok((main_branch_of(project)?, true))
}

fn target_worktrees(repo: &RegisteredRepo) -> Result<Vec<crate::worktree::Worktree>, String> {
    crate::worktree::list(&repo.dirs[0]).ok_or_else(|| {
        format!(
            "'{}' is registered at {} but is not inside a git repository",
            repo.name,
            repo.dirs[0].display()
        )
    })
}

/// Branch of the target worktree that matches `owner` (raw name first, then
/// slug). `None` when the repo has no such worktree.
fn pick_branch(
    worktrees: &[crate::worktree::Worktree],
    owner: &str,
) -> Result<Option<String>, String> {
    if let Some(exact) = worktrees
        .iter()
        .find(|worktree| worktree.branch.as_deref() == Some(owner))
    {
        return Ok(exact.branch.clone());
    }
    let Ok(slug) = ports::branch_slug(owner) else {
        return Ok(None);
    };
    let mut slugged: Vec<&str> = worktrees
        .iter()
        .filter_map(|worktree| worktree.branch.as_deref())
        .filter(|branch| ports::branch_slug(branch).is_ok_and(|b| b == slug))
        .collect();
    match slugged.len() {
        0 => Ok(None),
        1 => Ok(slugged.pop().map(str::to_string)),
        _ => {
            slugged.sort_unstable();
            Err(format!(
                "several worktrees match branch '{owner}': {}; use a concrete branch",
                slugged.join(", ")
            ))
        }
    }
}

/// One link in the auto-start chain: the canonical identity of an instance.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct ChainEntry {
    /// Git common dir (project identity), or the config dir outside git.
    project: String,
    /// Raw branch name; empty when unknown.
    branch: String,
    script: String,
}

impl ChainEntry {
    fn serialize(&self) -> String {
        format!(
            "{}{CHAIN_FIELD}{}{CHAIN_FIELD}{}",
            self.project, self.branch, self.script
        )
    }

    fn parse(line: &str) -> Option<Self> {
        let mut fields = line.split(CHAIN_FIELD);
        let project = fields.next()?.to_string();
        let branch = fields.next()?.to_string();
        let script = fields.next()?.to_string();
        if fields.next().is_some() || project.is_empty() || script.is_empty() {
            return None;
        }
        Some(Self {
            project,
            branch,
            script,
        })
    }

    /// Human-readable `repo:branch:script` label.
    fn label(&self) -> String {
        // `display_name` also passes a plain (already short) project name
        // through unchanged, so it works for both the git identity path and a
        // non-git fallback without guessing the separator: identities are
        // always `/`-separated, which is not `MAIN_SEPARATOR` on Windows.
        let repo = crate::project::display_name(&self.project);
        if self.branch.is_empty() {
            format!("{repo}:{script}", script = self.script)
        } else {
            format!("{repo}:{}:{}", self.branch, self.script)
        }
    }
}

/// The chain of instances that led to this process, oldest first.
#[derive(Debug, Clone, Default)]
struct StartChain {
    entries: Vec<ChainEntry>,
}

impl StartChain {
    fn from_env() -> Self {
        let Ok(raw) = std::env::var(AUTOSTART_CHAIN_ENV) else {
            return Self::default();
        };
        Self {
            entries: raw.lines().filter_map(ChainEntry::parse).collect(),
        }
    }

    fn push(&mut self, entry: ChainEntry) {
        if !self.contains(&entry) {
            self.entries.push(entry);
        }
    }

    fn contains(&self, entry: &ChainEntry) -> bool {
        self.entries.iter().any(|known| known == entry)
    }

    fn serialize(&self) -> String {
        self.entries
            .iter()
            .map(ChainEntry::serialize)
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn labels(&self) -> Vec<String> {
        self.entries.iter().map(ChainEntry::label).collect()
    }
}

/// The instance this process is resolving references for.
fn own_identity(owner: Owner<'_>) -> ChainEntry {
    let project = owner
        .project
        .map(str::to_string)
        .or_else(|| {
            owner
                .config_dir
                .canonicalize()
                .ok()
                .map(|p| p.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| owner.config_dir.to_string_lossy().into_owned());
    ChainEntry {
        project,
        branch: owner.branch.unwrap_or_default().to_string(),
        script: owner.script_name.to_string(),
    }
}

fn cycle_error(references: &[Request], chain: &StartChain, target: &Target) -> String {
    let mut path = chain.labels();
    path.push(target.label());
    format!(
        "'${{{}}}' would start '{}' again; cross-instance start cycle: {} — start one of those \
         instances first",
        references[0].original.atom(),
        target.label(),
        path.join(" -> ")
    )
}

/// A registered project, grouped by git identity across its worktrees.
#[derive(Debug, Clone)]
struct RegisteredRepo {
    identity: String,
    name: String,
    /// Registered config dirs belonging to this repository.
    dirs: Vec<PathBuf>,
}

/// A project+branch+script that can be started to satisfy a reference.
#[derive(Debug, Clone)]
struct Target {
    identity: String,
    name: String,
    branch: String,
    script: String,
    config_path: PathBuf,
}

impl Target {
    fn identity(&self) -> ChainEntry {
        ChainEntry {
            project: self.identity.clone(),
            branch: self.branch.clone(),
            script: self.script.clone(),
        }
    }

    fn label(&self) -> String {
        format!("{}:{}:{}", self.name, self.branch, self.script)
    }

    fn resolve(reference: &ExternalPortRef) -> Result<Self, String> {
        let script = reference
            .script
            .as_deref()
            .expect("cold start requires a script segment");
        let repo = find_repo(&reference.project)?;
        let worktrees = worktree::list(&repo.dirs[0]).ok_or_else(|| {
            format!(
                "cross-instance port '{}': '{}' is registered at {} but is not inside a git \
                 repository",
                reference.atom(),
                repo.name,
                repo.dirs[0].display()
            )
        })?;
        let selected = select_worktree(&worktrees, &reference.branch, &repo)?;
        let branch = selected.branch.clone().unwrap_or_default();
        let config_path = find_config_path(&repo, &worktrees, selected, reference)?;
        let config = crate::config::load(&config_path).map_err(|e| {
            format!(
                "cross-instance port '{}': cannot read {}: {e}",
                reference.atom(),
                config_path.display()
            )
        })?;
        let Some(script_config) = config.scripts.get(script) else {
            let mut scripts: Vec<&String> = config.scripts.keys().collect();
            scripts.sort();
            return Err(format!(
                "cross-instance port '{}': script '{}' does not exist in {} (scripts: {})",
                reference.atom(),
                script,
                config_path.display(),
                list_or_none(&scripts)
            ));
        };
        if !declares_port(&config, script_config, &reference.name) {
            let mut names: BTreeSet<String> = config
                .ports
                .as_ref()
                .map(|ports| ports.keys().cloned().collect())
                .unwrap_or_default();
            names.extend(ports::referenced_local_port_names(
                script_config,
                config.native_routes.as_ref(),
            ));
            let names: Vec<&String> = names.iter().collect();
            return Err(format!(
                "cross-instance port '{}': script '{}' in {} does not declare port '{}' \
                 (available: {})",
                reference.atom(),
                script,
                config_path.display(),
                reference.name,
                list_or_none(&names)
            ));
        }
        Ok(Self {
            identity: repo.identity,
            name: repo.name,
            branch,
            script: script.to_string(),
            config_path,
        })
    }
}

fn list_or_none(names: &[&String]) -> String {
    if names.is_empty() {
        "(none)".to_string()
    } else {
        names
            .iter()
            .map(|name| name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Whether `script` declares (or references) a local port named `name`.
fn declares_port(config: &crate::config::Config, script: &ScriptConfig, name: &str) -> bool {
    config
        .ports
        .as_ref()
        .is_some_and(|ports| ports.contains_key(name))
        || ports::referenced_local_port_names(script, config.native_routes.as_ref()).contains(name)
}

fn find_repo(token: &str) -> Result<RegisteredRepo, String> {
    let repos = registered_repos();
    let matching: Vec<&RegisteredRepo> = repos
        .iter()
        .filter(|repo| ports::project_matches(&repo.identity, token))
        .collect();
    match matching.len() {
        1 => Ok(matching[0].clone()),
        0 => {
            let names: Vec<String> = repos.iter().map(|repo| repo.name.clone()).collect();
            Err(format!(
                "cross-instance port: no registered fog project matches '{token}' (registered: \
                 {}) — run that project once or add it in the UI, or pass --port to provide the \
                 value",
                list_or_none(&names.iter().collect::<Vec<_>>())
            ))
        }
        _ => Err(format!(
            "cross-instance port: project '{token}' is ambiguous between {} — pass --port to \
             choose the value",
            matching
                .iter()
                .map(|repo| format!("{} ({})", repo.name, repo.identity))
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// The registered repos whose directories still exist, grouped by identity.
fn registered_repos() -> Vec<RegisteredRepo> {
    let mut repos: BTreeMap<String, RegisteredRepo> = BTreeMap::new();
    for dir in crate::index::registry::registered_config_dirs() {
        let Some(identity) = crate::project::detect(&dir) else {
            continue;
        };
        let name = crate::project::display_name(&identity);
        repos
            .entry(identity.clone())
            .or_insert_with(|| RegisteredRepo {
                identity,
                name,
                dirs: Vec::new(),
            })
            .dirs
            .push(dir);
    }
    repos.into_values().collect()
}

fn select_worktree<'a>(
    worktrees: &'a [Worktree],
    token: &str,
    repo: &RegisteredRepo,
) -> Result<&'a Worktree, String> {
    let matching: Vec<&Worktree> = worktrees
        .iter()
        .filter(|worktree| {
            worktree
                .branch
                .as_deref()
                .is_some_and(|branch| ports::branch_matches(branch, token))
        })
        .collect();
    match matching.len() {
        1 => Ok(matching[0]),
        0 => {
            let mut branches: Vec<String> = worktrees
                .iter()
                .filter_map(|worktree| worktree.branch.clone())
                .collect();
            branches.sort();
            let branches: Vec<&String> = branches.iter().collect();
            Err(format!(
                "cross-instance port: '{}' has no worktree on branch '{}' (branches: {})",
                repo.name,
                token,
                list_or_none(&branches)
            ))
        }
        _ => Err(format!(
            "cross-instance port: branch '{}' of '{}' is ambiguous between {} — use the full branch \
             name",
            token,
            repo.name,
            matching
                .iter()
                .map(|worktree| worktree.label())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// Finds the `fog.json` for `selected` by mirroring each registered config dir's
/// subpath inside its own worktree onto the target worktree.
fn find_config_path(
    repo: &RegisteredRepo,
    worktrees: &[Worktree],
    selected: &Worktree,
    reference: &ExternalPortRef,
) -> Result<PathBuf, String> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    for dir in &repo.dirs {
        let Some(containing) = worktree::containing_worktree(worktrees, dir) else {
            continue;
        };
        let base = containing
            .path
            .canonicalize()
            .unwrap_or_else(|_| containing.path.clone());
        let dir = dir.canonicalize().unwrap_or_else(|_| dir.clone());
        let Ok(suffix) = dir.strip_prefix(&base) else {
            continue;
        };
        let candidate = selected.path.join(suffix).join("fog.json");
        if candidate.is_file() {
            candidates.push(candidate);
        }
    }
    candidates.sort();
    candidates.dedup();
    match candidates.len() {
        1 => Ok(candidates.into_iter().next().expect("one candidate")),
        0 => Err(format!(
            "cross-instance port '{}': branch '{}' of '{}' has no fog.json at the registered \
             config location",
            reference.atom(),
            selected.branch.as_deref().unwrap_or("?"),
            repo.name
        )),
        _ => Err(format!(
            "cross-instance port '{}': branch '{}' of '{}' has several fog.json candidates ({}) — \
             pass --port to choose the value",
            reference.atom(),
            selected.branch.as_deref().unwrap_or("?"),
            repo.name,
            candidates
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// Best-effort per-target lock so two parents do not start the same instance.
fn start_lock_path(entry: &ChainEntry) -> PathBuf {
    // FNV-1a: deterministic across processes, and the path only needs to be
    // unique per target.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in entry.serialize().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    crate::ipc::instance_dir().join(format!("autostart-{hash:016x}.lock"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(project: &str, branch: &str, script: &str) -> ChainEntry {
        ChainEntry {
            project: project.to_string(),
            branch: branch.to_string(),
            script: script.to_string(),
        }
    }

    #[test]
    fn test_cycle_error_names_the_full_path() {
        let reference = ExternalPortRef {
            name: "api".to_string(),
            project: "b".to_string(),
            branch: "main".to_string(),
            script: Some("dev".to_string()),
        };
        let requests = vec![Request {
            original: reference.clone(),
            matcher: reference,
            note: None,
        }];
        let mut chain = StartChain::default();
        chain.push(entry("/repo/a/.git", "main", "dev"));
        chain.push(entry("/repo/b/.git", "main", "dev"));
        let target = Target {
            identity: "/repo/a/.git".to_string(),
            name: "a".to_string(),
            branch: "main".to_string(),
            script: "dev".to_string(),
            config_path: PathBuf::from("/repo/a/fog.json"),
        };
        let message = cycle_error(&requests, &chain, &target);
        assert!(message.contains("${ports.api@b:main:dev}"), "{message}");
        assert!(
            message.contains("cross-instance start cycle: a:main:dev -> b:main:dev -> a:main:dev"),
            "{message}"
        );
    }

    #[test]
    fn test_chain_entry_round_trip() {
        let entry = entry("/repo/.git", "feat/login", "dev");
        let line = entry.serialize();
        assert_eq!(ChainEntry::parse(&line), Some(entry));
        assert_eq!(ChainEntry::parse("only-one-field"), None);
        assert_eq!(ChainEntry::parse(""), None);
    }

    #[test]
    fn test_start_chain_dedupes_and_labels() {
        let mut chain = StartChain::default();
        chain.push(entry("/Users/me/dev/cinema_ticket/.git", "main", "dev"));
        chain.push(entry("/Users/me/dev/cinema_ticket/.git", "main", "dev"));
        assert_eq!(chain.entries.len(), 1);
        assert_eq!(chain.labels(), vec!["cinema_ticket:main:dev"]);
        assert!(chain.contains(&entry("/Users/me/dev/cinema_ticket/.git", "main", "dev")));
    }

    #[test]
    fn test_start_chain_serialize_round_trip() {
        let mut chain = StartChain::default();
        chain.push(entry("/repo/.git", "main", "dev"));
        chain.push(entry("/other/.git", "", "api"));
        let parsed = StartChain {
            entries: chain
                .serialize()
                .lines()
                .filter_map(ChainEntry::parse)
                .collect(),
        };
        assert_eq!(parsed.entries, chain.entries);
        assert_eq!(parsed.labels(), vec!["repo:main:dev", "other:api"]);
    }

    #[test]
    fn test_own_identity_uses_project_identity() {
        let own = own_identity(Owner {
            config_dir: Path::new("/Users/me/dev/repo/app"),
            project: Some("/Users/me/dev/repo/.git"),
            branch: Some("main"),
            script_name: "dev",
        });
        assert_eq!(own.project, "/Users/me/dev/repo/.git");
        assert_eq!(own.branch, "main");
        assert_eq!(own.script, "dev");
    }

    #[test]
    fn test_concrete_branch_expands_branch_atom_but_keeps_key() {
        let reference = ExternalPortRef {
            name: "api".to_string(),
            project: "b".to_string(),
            branch: "${branch}".to_string(),
            script: Some("dev".to_string()),
        };
        let owner = Owner {
            config_dir: Path::new("/repo/a"),
            project: None,
            branch: Some("feat/login"),
            script_name: "web",
        };
        let request = request_for(reference, owner).unwrap();
        assert_eq!(request.matcher.branch, "feat-login");
        // The template key keeps the atom so `resolve_template` finds it.
        assert_eq!(request.original.key(), "api@b:${branch}:dev");
        assert_eq!(request.matcher.key(), "api@b:feat-login:dev");
    }

    #[test]
    fn test_concrete_branch_without_owner_branch_errors() {
        let reference = ExternalPortRef {
            name: "api".to_string(),
            project: "b".to_string(),
            branch: "${branch}".to_string(),
            script: None,
        };
        let owner = Owner {
            config_dir: Path::new("/repo/a"),
            project: None,
            branch: None,
            script_name: "web",
        };
        let err = request_for(reference, owner).unwrap_err();
        assert!(err.contains("detached HEAD"), "{err}");
        assert!(err.contains("--port"), "{err}");
    }

    #[test]
    fn test_pick_branch_prefers_exact_then_slug() {
        let worktree = |branch: &str| crate::worktree::Worktree {
            path: PathBuf::from(format!("/repo/{branch}")),
            branch: Some(branch.to_string()),
        };
        let trees = vec![worktree("main"), worktree("feat/login")];
        assert_eq!(
            pick_branch(&trees, "feat/login").unwrap(),
            Some("feat/login".to_string())
        );
        assert_eq!(pick_branch(&trees, "release/2.0").unwrap(), None);

        let slugged = vec![worktree("main"), worktree("feat-login")];
        assert_eq!(
            pick_branch(&slugged, "feat/login").unwrap(),
            Some("feat-login".to_string())
        );

        let detached = vec![
            worktree("main"),
            crate::worktree::Worktree {
                path: PathBuf::from("/repo/detached"),
                branch: None,
            },
        ];
        assert_eq!(pick_branch(&detached, "feat/login").unwrap(), None);
    }

    #[test]
    fn test_pick_branch_rejects_ambiguous_slug_matches() {
        let worktree = |branch: &str| crate::worktree::Worktree {
            path: PathBuf::from(format!("/repo/{branch}")),
            branch: Some(branch.to_string()),
        };
        let trees = vec![worktree("feat-login"), worktree("feat/login")];
        let err = pick_branch(&trees, "feat.login").unwrap_err();
        assert!(err.contains("several worktrees"), "{err}");
        assert!(err.contains("feat-lo"), "{err}");
    }

    #[test]
    fn test_resolved_message_includes_fallback_note() {
        let reference = ExternalPortRef {
            name: "api".to_string(),
            project: "b".to_string(),
            branch: "?".to_string(),
            script: Some("dev".to_string()),
        };
        let request = Request {
            original: reference.clone(),
            matcher: ExternalPortRef {
                branch: "main".to_string(),
                ..reference
            },
            note: Some("fell back to 'main'".to_string()),
        };
        assert_eq!(
            resolved_message(&request, 8080, 42),
            "resolved 'ports.api@b:?:dev' = 8080 (pid 42, fell back to 'main')"
        );
    }

    #[test]
    fn test_target_label_and_identity() {
        let target = Target {
            identity: "/repo/.git".to_string(),
            name: "repo".to_string(),
            branch: "main".to_string(),
            script: "dev".to_string(),
            config_path: PathBuf::from("/repo/fog.json"),
        };
        assert_eq!(target.label(), "repo:main:dev");
        assert_eq!(target.identity(), entry("/repo/.git", "main", "dev"));
    }

    #[test]
    fn test_start_lock_path_is_stable_per_target() {
        let a = start_lock_path(&entry("/repo/.git", "main", "dev"));
        let b = start_lock_path(&entry("/repo/.git", "main", "dev"));
        let c = start_lock_path(&entry("/repo/.git", "dev", "main"));
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn test_find_repo_matches_names_case_insensitively() {
        // `find_repo` reads the real registry; the matching rule it applies is
        // covered by `project_matches`, tested in ports.rs. Here only the
        // empty-registry error shape is asserted to stay actionable.
        let err = find_repo("definitely-not-a-registered-repo").unwrap_err();
        assert!(err.contains("registered"), "{err}");
        assert!(err.contains("--port"), "{err}");
    }
}
