use std::path::{Path, PathBuf};

/// Resolves the canonical path of a possibly-relative git dir.
fn canonicalize_path(base: &Path, value: &str) -> Option<String> {
    let p = Path::new(value);
    let absolute = if p.is_absolute() {
        p.to_path_buf()
    } else {
        base.join(p)
    };
    absolute
        .canonicalize()
        .ok()
        .map(|c| c.to_string_lossy().into_owned())
}

/// Detects the identity of the git repository containing `config_dir`.
///
/// Uses `git rev-parse --git-common-dir`, which returns the same value for
/// every worktree of the same repository. Returns `None` when `config_dir`
/// is not inside a git repository.
pub fn detect(config_dir: &Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(config_dir)
        .args(["rev-parse", "--git-common-dir"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if value.is_empty() {
        return None;
    }
    canonicalize_path(config_dir, &value)
}

/// Returns a human-readable label for the project (the git top-level).
///
/// Falls back to the config directory when not inside a git repository.
pub fn display(config_dir: &Path) -> String {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(config_dir)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok();
    if let Some(out) = out
        && out.status.success()
    {
        let value = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !value.is_empty() {
            return value;
        }
    }
    config_dir.to_string_lossy().into_owned()
}

/// Returns the main working-tree root of the git repository containing `dir`,
/// regardless of which worktree `dir` is checked out in.
///
/// Linked worktrees all share the main repository's git common dir (e.g.
/// `/repo/.git`); its parent is the main working tree. Returns `None` when
/// `dir` is not inside a git repository.
pub fn repo_root(dir: &Path) -> Option<PathBuf> {
    let common = PathBuf::from(detect(dir)?);
    common.parent().map(Path::to_path_buf)
}

/// Fallback identity when the directory is not a git repository: the
/// canonicalized config directory path.
pub fn fallback_identity(config_dir: &Path) -> Option<String> {
    config_dir
        .canonicalize()
        .ok()
        .map(|c| c.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;

    #[test]
    fn test_detect_git_common_dir_shared_across_worktrees() {
        let git = Command::new("git").arg("--version").output();
        if git.map(|o| !o.status.success()).unwrap_or(true) {
            eprintln!("skipping: git not available");
            return;
        }

        let base = std::env::temp_dir().join(format!("fog-project-test-{}", std::process::id()));
        let repo = base.join("repo");
        fs::create_dir_all(&repo).unwrap();

        let run = |dir: &Path, args: &[&str]| {
            Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .output()
                .map(|o| {
                    (
                        o.status.success(),
                        String::from_utf8_lossy(&o.stdout).trim().to_string(),
                    )
                })
                .unwrap_or((false, String::new()))
        };

        run(&repo, &["init", "-q", "-b", "main"]);
        run(&repo, &["config", "user.email", "test@example.com"]);
        run(&repo, &["config", "user.name", "Test"]);
        fs::write(repo.join("fog.json"), "{}").unwrap();
        run(&repo, &["add", "fog.json"]);
        run(&repo, &["commit", "-q", "-m", "init"]);

        let worktree = base.join("worktree");
        let added = run(
            &repo,
            &["worktree", "add", "-q", &worktree.to_string_lossy()],
        );
        if !added.0 {
            eprintln!("skipping: could not add worktree");
            let _ = fs::remove_dir_all(&base);
            return;
        }

        let repo_id = detect(&repo);
        let worktree_id = detect(&worktree);
        assert!(
            repo_id.is_some(),
            "identity should be detected in a git repo"
        );
        assert_eq!(repo_id, worktree_id, "worktrees must share identity");

        let _ = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["worktree", "remove", "--force", &worktree.to_string_lossy()])
            .output();
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn test_repo_root_is_main_worktree_across_worktrees() {
        let git = Command::new("git").arg("--version").output();
        if git.map(|o| !o.status.success()).unwrap_or(true) {
            eprintln!("skipping: git not available");
            return;
        }

        let base = std::env::temp_dir().join(format!("fog-root-test-{}", std::process::id()));
        let repo = base.join("red-fox");
        fs::create_dir_all(&repo).unwrap();

        let run = |dir: &Path, args: &[&str]| {
            Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .output()
                .map(|o| {
                    (
                        o.status.success(),
                        String::from_utf8_lossy(&o.stdout).trim().to_string(),
                    )
                })
                .unwrap_or((false, String::new()))
        };

        run(&repo, &["init", "-q", "-b", "main"]);
        run(&repo, &["config", "user.email", "test@example.com"]);
        run(&repo, &["config", "user.name", "Test"]);
        fs::write(repo.join("fog.json"), "{}").unwrap();
        run(&repo, &["add", "fog.json"]);
        run(&repo, &["commit", "-q", "-m", "init"]);

        let worktree = base.join("ui");
        let added = run(
            &repo,
            &["worktree", "add", "-q", &worktree.to_string_lossy()],
        );
        if !added.0 {
            eprintln!("skipping: could not add worktree");
            let _ = fs::remove_dir_all(&base);
            return;
        }

        // The worktree checkout dir is named after its branch, not the repo,
        // but both must resolve to the same main working-tree root.
        let root = repo_root(&repo);
        let wt_root = repo_root(&worktree);
        assert!(root.is_some(), "root should resolve inside a git repo");
        assert_eq!(
            root, wt_root,
            "linked worktrees must resolve to the main working-tree root"
        );
        assert_eq!(
            root.unwrap()
                .file_name()
                .map(|n| n.to_string_lossy().into_owned()),
            Some("red-fox".to_string())
        );

        let _ = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["worktree", "remove", "--force", &worktree.to_string_lossy()])
            .output();
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn test_detect_none_outside_git() {
        let dir = std::env::temp_dir().join(format!("fog-no-git-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        assert!(detect(&dir).is_none());
        assert_eq!(
            fallback_identity(&dir),
            dir.canonicalize()
                .ok()
                .map(|c| c.to_string_lossy().into_owned())
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
