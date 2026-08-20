use crate::worktree::Worktree;

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
