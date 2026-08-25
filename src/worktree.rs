use std::path::{Path, PathBuf};

/// A git worktree discovered via `git worktree list --porcelain`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worktree {
    /// Absolute path to the worktree's working directory.
    pub path: PathBuf,
    /// Branch checked out in the worktree, or `None` when detached.
    pub branch: Option<String>,
}

impl Worktree {
    /// Human-readable label for completion/listing: the branch when checked
    /// out, otherwise the path.
    pub fn label(&self) -> String {
        self.branch
            .clone()
            .unwrap_or_else(|| self.path.to_string_lossy().into_owned())
    }

    /// Returns `true` when `path` is inside this worktree's working directory.
    pub fn contains(&self, path: &Path) -> bool {
        let wt = self
            .path
            .canonicalize()
            .unwrap_or_else(|_| self.path.clone());
        let p = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        p.starts_with(&wt)
    }

    /// Returns `true` when this worktree's path equals `other` after
    /// canonicalization (resolves symlinks like `/Users` on macOS). Used to
    /// decide "already on this branch" without false-positives for nested
    /// worktrees (where a parent `contains` a child path).
    pub fn path_equals(&self, other: &Path) -> bool {
        let a = self
            .path
            .canonicalize()
            .unwrap_or_else(|_| self.path.clone());
        let b = other.canonicalize().unwrap_or_else(|_| other.to_path_buf());
        a == b
    }
}

/// Lists every worktree of the git repository containing `config_dir`.
///
/// Returns `None` when `config_dir` is not inside a git repository.
pub fn list(config_dir: &Path) -> Option<Vec<Worktree>> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(config_dir)
        .args(["worktree", "list", "--porcelain"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let output = String::from_utf8_lossy(&out.stdout).into_owned();
    Some(parse_porcelain(&output))
}

/// Parses the output of `git worktree list --porcelain`.
///
/// Records are separated by blank lines. Each record has a `worktree <path>`
/// line and either a `branch refs/heads/<name>` line or a `detached` line.
fn parse_porcelain(output: &str) -> Vec<Worktree> {
    let mut worktrees = Vec::new();
    let mut path: Option<PathBuf> = None;
    let mut branch: Option<String> = None;

    for line in output.lines() {
        if line.trim().is_empty() {
            if let Some(p) = path.take() {
                worktrees.push(Worktree {
                    path: p,
                    branch: branch.take(),
                });
            }
            continue;
        }
        if let Some(value) = line.strip_prefix("worktree ") {
            path = Some(PathBuf::from(value.trim()));
        } else if let Some(value) = line.strip_prefix("branch refs/heads/") {
            branch = Some(value.trim().to_string());
        }
        // `detached`, `bare`, and `prunable` lines carry no useful data here.
    }
    if let Some(p) = path.take() {
        worktrees.push(Worktree {
            path: p,
            branch: branch.take(),
        });
    }
    worktrees
}

/// Finds the worktree checked out on `branch`, if any.
pub fn resolve(config_dir: &Path, branch: &str) -> Option<Worktree> {
    list(config_dir)?
        .into_iter()
        .find(|w| w.branch.as_deref() == Some(branch))
}

/// Returns the worktree that most specifically contains `path`, if any.
///
/// When worktrees are nested (e.g. `./wt-feature` inside the main checkout),
/// more than one entry's `contains` will be true; this picks the deepest
/// (longest canonical path) so we identify the *actual* worktree the path
/// lives in rather than an ancestor.
pub fn containing_worktree<'a>(worktrees: &'a [Worktree], path: &Path) -> Option<&'a Worktree> {
    let mut best: Option<&'a Worktree> = None;
    let mut best_len: Option<usize> = None;
    for wt in worktrees {
        if wt.contains(path) {
            let len = wt
                .path
                .canonicalize()
                .unwrap_or_else(|_| wt.path.clone())
                .components()
                .count();
            if best_len.is_none_or(|l| len > l) {
                best = Some(wt);
                best_len = Some(len);
            }
        }
    }
    best
}

/// Returns `true` when `candidate` is the worktree that actually contains
/// `path` (the deepest `contains` match). This is the correct test for
/// "already on this worktree" — a plain `candidate.contains(path)` is true
/// for every ancestor of a nested worktree.
pub fn is_current_worktree(candidate: &Worktree, worktrees: &[Worktree], path: &Path) -> bool {
    match containing_worktree(worktrees, path) {
        Some(cur) => {
            let cur_can = cur.path.canonicalize().unwrap_or_else(|_| cur.path.clone());
            let cand_can = candidate
                .path
                .canonicalize()
                .unwrap_or_else(|_| candidate.path.clone());
            cur_can == cand_can
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
worktree /repo/fog
HEAD aaaa
branch refs/heads/main

worktree /repo/fog-feature-x
HEAD bbbb
branch refs/heads/feature-x

worktree /repo/fog-detached
HEAD cccc
detached
";

    #[test]
    fn test_parse_porcelain_full() {
        let wts = parse_porcelain(SAMPLE);
        assert_eq!(wts.len(), 3);
        assert_eq!(wts[0].path, PathBuf::from("/repo/fog"));
        assert_eq!(wts[0].branch.as_deref(), Some("main"));
        assert_eq!(wts[1].branch.as_deref(), Some("feature-x"));
        assert_eq!(wts[2].branch, None, "detached worktrees have no branch");
    }

    #[test]
    fn test_parse_porcelain_empty() {
        assert!(parse_porcelain("").is_empty());
        assert!(parse_porcelain("\n\n").is_empty());
    }

    #[test]
    fn test_parse_porcelain_bare_flag() {
        // A worktree with `prunable` and `bare` lines must still parse.
        let out = "\
worktree /repo/fog
HEAD aaaa
branch refs/heads/main
bare

worktree /repo/fog-old
HEAD dddd
branch refs/heads/old
prunable gitdir file points to non-existent location
";
        let wts = parse_porcelain(out);
        assert_eq!(wts.len(), 2);
        assert_eq!(wts[0].branch.as_deref(), Some("main"));
    }

    #[test]
    fn test_label() {
        let wt = Worktree {
            path: PathBuf::from("/repo/fog"),
            branch: Some("main".to_string()),
        };
        assert_eq!(wt.label(), "main");
        let detached = Worktree {
            path: PathBuf::from("/repo/fog-detached"),
            branch: None,
        };
        assert_eq!(detached.label(), "/repo/fog-detached");
    }

    #[test]
    fn test_contains() {
        let wt = Worktree {
            path: PathBuf::from("/repo/fog"),
            branch: Some("main".to_string()),
        };
        assert!(wt.contains(Path::new("/repo/fog")));
        assert!(wt.contains(Path::new("/repo/fog/sub")));
        assert!(!wt.contains(Path::new("/repo/fog-feature")));
    }

    #[test]
    fn test_containing_worktree_picks_deepest_for_nested() {
        let wts = vec![
            Worktree {
                path: PathBuf::from("/repo/fog"),
                branch: Some("main".to_string()),
            },
            Worktree {
                path: PathBuf::from("/repo/fog/wt-feature"),
                branch: Some("feature".to_string()),
            },
        ];
        // Inside nested worktree — should pick the nested one, not the ancestor
        let cur = containing_worktree(&wts, Path::new("/repo/fog/wt-feature")).unwrap();
        assert_eq!(cur.branch.as_deref(), Some("feature"));
        let cur = containing_worktree(&wts, Path::new("/repo/fog/wt-feature/sub")).unwrap();
        assert_eq!(cur.branch.as_deref(), Some("feature"));
        // Inside main — should pick main
        let cur = containing_worktree(&wts, Path::new("/repo/fog")).unwrap();
        assert_eq!(cur.branch.as_deref(), Some("main"));
        // Sibling not inside either's ancestor
        let wts2 = vec![
            Worktree {
                path: PathBuf::from("/repo/fog"),
                branch: Some("main".to_string()),
            },
            Worktree {
                path: PathBuf::from("/repo/fog-feature"),
                branch: Some("feature".to_string()),
            },
        ];
        assert!(
            containing_worktree(&wts2, Path::new("/repo/fog"))
                .unwrap()
                .branch
                == Some("main".to_string())
        );
        assert!(containing_worktree(&wts2, Path::new("/repo/other")).is_none());
    }

    #[test]
    fn test_is_current_worktree_nested_false_positive_fix() {
        let wts = vec![
            Worktree {
                path: PathBuf::from("/repo/fog"),
                branch: Some("main".to_string()),
            },
            Worktree {
                path: PathBuf::from("/repo/fog/wt-feature"),
                branch: Some("feature".to_string()),
            },
        ];
        let config_dir_in_nested = Path::new("/repo/fog/wt-feature");
        // Parent contains the nested path, but is NOT the current worktree
        assert!(wts[0].contains(config_dir_in_nested));
        assert!(!is_current_worktree(&wts[0], &wts, config_dir_in_nested));
        // Nested worktree is the current one
        assert!(is_current_worktree(&wts[1], &wts, config_dir_in_nested));
        // From main, nested is not current
        let config_dir_in_main = Path::new("/repo/fog");
        assert!(is_current_worktree(&wts[0], &wts, config_dir_in_main));
        assert!(!is_current_worktree(&wts[1], &wts, config_dir_in_main));
    }

    #[test]
    fn test_is_current_worktree_subdir_same_worktree() {
        // fog.json in a subdir (e.g. infra/) is still the same worktree
        let wts = vec![Worktree {
            path: PathBuf::from("/repo/fog"),
            branch: Some("main".to_string()),
        }];
        assert!(is_current_worktree(
            &wts[0],
            &wts,
            Path::new("/repo/fog/infra")
        ));
        // But a sibling worktree is not considered current
        let wts2 = vec![
            Worktree {
                path: PathBuf::from("/repo/fog"),
                branch: Some("main".to_string()),
            },
            Worktree {
                path: PathBuf::from("/repo/fog-feature"),
                branch: Some("feature".to_string()),
            },
        ];
        assert!(!is_current_worktree(
            &wts2[1],
            &wts2,
            Path::new("/repo/fog/infra")
        ));
    }
}
