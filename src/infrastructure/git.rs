use std::collections::HashSet;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::Mutex;

use anyhow::Context as _;
use tokio::process::Command;

use crate::types::{WorktreeCheckResult, WorktreeCleanResult};

/// Manages Git repository discovery and isolated temporary worktree lifecycles.
pub struct GitService {
    active_worktrees: Mutex<HashSet<PathBuf>>,
}

impl GitService {
    pub fn new() -> Self {
        Self {
            active_worktrees: Mutex::new(HashSet::new()),
        }
    }

    async fn run_git(dir: &Path, args: &[&str]) -> anyhow::Result<Output> {
        Command::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .await
            .with_context(|| format!("failed to execute git {} in {}", args.join(" "), dir.display()))
    }

    /// Returns `true` if `path` is inside a Git working tree.
    pub async fn check_git_repo(&self, path: &Path) -> bool {
        if !path.is_dir() {
            return false;
        }
        match Self::run_git(path, &["rev-parse", "--is-inside-work-tree"]).await {
            Ok(out) => {
                out.status.success() && String::from_utf8_lossy(&out.stdout).trim() == "true"
            }
            Err(_) => false,
        }
    }

    /// Resolves the top-level Git repository root containing `path`.
    pub async fn get_repo_root(&self, path: &Path) -> anyhow::Result<PathBuf> {
        let out = Self::run_git(path, &["rev-parse", "--show-toplevel"]).await?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            anyhow::bail!("Not a Git repository ({stderr})");
        }
        let root = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if root.is_empty() {
            anyhow::bail!("git rev-parse --show-toplevel returned an empty path");
        }
        Ok(PathBuf::from(root))
    }

    /// Fetches `origin <branch>` and returns `FETCH_HEAD` SHA, falling back to `<branch>` on offline/local errors.
    pub async fn fetch_branch_ref(&self, repo_root: &Path, branch: &str) -> String {
        if let Ok(fetch_out) = Self::run_git(repo_root, &["fetch", "origin", branch]).await {
            if fetch_out.status.success() {
                if let Ok(rev_out) = Self::run_git(repo_root, &["rev-parse", "FETCH_HEAD"]).await {
                    if rev_out.status.success() {
                        let sha = String::from_utf8_lossy(&rev_out.stdout).trim().to_string();
                        if !sha.is_empty() {
                            return sha;
                        }
                    }
                }
            }
        }
        branch.to_string()
    }

    /// Sanitizes a repository directory name for safe use inside worktree directory names.
    pub fn sanitize_repo_name(repo_root: &Path) -> String {
        let raw = repo_root
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("repo");
        let sanitized: String = raw
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        if sanitized.is_empty() {
            "repo".to_string()
        } else {
            sanitized
        }
    }

    /// Creates a detached temporary worktree at `worktree_path` checked out to `commit_or_ref`.
    pub async fn add_worktree(
        &self,
        repo_root: &Path,
        worktree_path: &Path,
        commit_or_ref: &str,
    ) -> anyhow::Result<()> {
        if let Some(parent) = worktree_path.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!(
                    "failed to create worktree base directory {}",
                    parent.display()
                )
            })?;
        }

        // Remove any stale directory/registration at this exact path first.
        self.remove_worktree(Some(repo_root), worktree_path).await;

        let wt_str = worktree_path.to_string_lossy().to_string();
        let out = Self::run_git(
            repo_root,
            &["worktree", "add", "--detach", &wt_str, commit_or_ref],
        )
        .await?;

        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            anyhow::bail!("Failed to create git worktree: {stderr}");
        }

        if let Ok(mut active) = self.active_worktrees.lock() {
            active.insert(worktree_path.to_path_buf());
            if let Ok(canon) = std::fs::canonicalize(worktree_path) {
                active.insert(canon);
            }
        }

        Ok(())
    }

    /// Force-removes a temporary worktree and falls back to filesystem deletion + `git worktree prune`.
    pub async fn remove_worktree(&self, repo_root: Option<&Path>, worktree_path: &Path) {
        let canon = std::fs::canonicalize(worktree_path).ok();
        if let Ok(mut active) = self.active_worktrees.lock() {
            active.remove(worktree_path);
            if let Some(ref c) = canon {
                active.remove(c);
            }
        }

        if let Some(root) = repo_root {
            let wt_str = worktree_path.to_string_lossy().to_string();
            let removed_cleanly = Self::run_git(root, &["worktree", "remove", "--force", &wt_str])
                .await
                .is_ok_and(|out| out.status.success());

            if !removed_cleanly {
                if worktree_path.exists() {
                    let _ = std::fs::remove_dir_all(worktree_path);
                }
                let _ = Self::run_git(root, &["worktree", "prune"]).await;
            }
        } else if worktree_path.exists() {
            let _ = std::fs::remove_dir_all(worktree_path);
        }
    }

    fn parse_worktree_parent_repo(worktree_dir: &Path) -> Option<PathBuf> {
        let dot_git = worktree_dir.join(".git");
        if !dot_git.is_file() {
            return None;
        }
        let content = std::fs::read_to_string(&dot_git).ok()?;
        let gitdir_raw = content
            .lines()
            .find_map(|line| line.trim().strip_prefix("gitdir:"))?
            .trim();
        if gitdir_raw.is_empty() {
            return None;
        }

        let raw_path = Path::new(gitdir_raw);
        let gitdir_path = if raw_path.is_absolute() {
            raw_path.to_path_buf()
        } else {
            worktree_dir.join(raw_path)
        };

        // Typical layout: <repo_root>/.git/worktrees/<worktree_name>
        let worktrees_dir = gitdir_path.parent()?;
        if worktrees_dir.file_name() != Some(OsStr::new("worktrees")) {
            return None;
        }
        let repo_dot_git = worktrees_dir.parent()?;
        if repo_dot_git.file_name() != Some(OsStr::new(".git")) {
            return None;
        }
        repo_dot_git.parent().map(Path::to_path_buf)
    }

    fn looks_like_stitch_worktree(dir: &Path) -> bool {
        if Self::parse_worktree_parent_repo(dir).is_some() {
            return true;
        }
        dir.file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|name| {
                name.contains("_tshirt_") || name.contains("_ticket_") || name.contains("_pr_")
            })
    }

    /// Scans `base_dir` for Stitch worktree directories not belonging to any active session.
    pub fn check_worktrees(&self, base_dir: &Path) -> WorktreeCheckResult {
        if !base_dir.is_dir() {
            return WorktreeCheckResult::default();
        }

        let active_snapshot = self
            .active_worktrees
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default();

        let Ok(entries) = std::fs::read_dir(base_dir) else {
            return WorktreeCheckResult::default();
        };

        let mut directories = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() || !Self::looks_like_stitch_worktree(&path) {
                continue;
            }
            let is_active = active_snapshot.contains(&path)
                || std::fs::canonicalize(&path)
                    .ok()
                    .is_some_and(|c| active_snapshot.contains(&c));
            if !is_active {
                directories.push(path);
            }
        }

        directories.sort();
        WorktreeCheckResult { directories }
    }

    /// Removes all orphaned worktree directories detected in `base_dir`.
    pub async fn clean_worktrees(&self, base_dir: &Path) -> WorktreeCleanResult {
        let check = self.check_worktrees(base_dir);
        let mut removed = 0usize;
        let mut errors = Vec::new();

        for dir in check.directories {
            let parent_repo = Self::parse_worktree_parent_repo(&dir);
            self.remove_worktree(parent_repo.as_deref(), &dir).await;

            if dir.exists() {
                // One final direct cleanup attempt in case git worktree remove left empty remnants.
                if let Err(err) = std::fs::remove_dir_all(&dir) {
                    errors.push(format!("{}: {err}", dir.display()));
                } else {
                    removed += 1;
                }
            } else {
                removed += 1;
            }
        }

        WorktreeCleanResult { removed, errors }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command as StdCommand;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn run_std_git(dir: &Path, args: &[&str]) {
        let out = StdCommand::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .expect("run git");
        assert!(
            out.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn git_worktree_lifecycle_and_orphan_cleanup() {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_nanos();
        let root_tmp = std::env::temp_dir().join(format!("stitch-rs-git-test-{nanos}"));
        let repo_dir = root_tmp.join("sample-repo");
        let sub_dir = repo_dir.join("nested");
        let wt_base = root_tmp.join("worktrees");

        std::fs::create_dir_all(&sub_dir).expect("create repo subdir");
        std::fs::create_dir_all(&wt_base).expect("create wt base");

        run_std_git(&repo_dir, &["init"]);
        run_std_git(&repo_dir, &["config", "user.name", "Stitch Test"]);
        run_std_git(&repo_dir, &["config", "user.email", "test@stitch.local"]);
        std::fs::write(repo_dir.join("README.md"), "hello").expect("write readme");
        run_std_git(&repo_dir, &["add", "."]);
        run_std_git(&repo_dir, &["commit", "-m", "init"]);

        let rt = crate::infrastructure::runtime::runtime();
        rt.block_on(async {
            let git = GitService::new();
            assert!(git.check_git_repo(&sub_dir).await);
            assert!(!git.check_git_repo(&root_tmp.join("missing")).await);

            let resolved_root = git.get_repo_root(&sub_dir).await.expect("repo root");
            assert_eq!(
                std::fs::canonicalize(&resolved_root).expect("canon"),
                std::fs::canonicalize(&repo_dir).expect("canon")
            );

            let wt_path = wt_base.join("sample-repo_tshirt_123");
            git.add_worktree(&resolved_root, &wt_path, "HEAD")
                .await
                .expect("add worktree");
            assert!(wt_path.join("README.md").is_file());

            // Active worktrees must not be flagged as orphaned by the owning GitService.
            assert!(git.check_worktrees(&wt_base).directories.is_empty());

            // A fresh GitService (simulating restart after crash) should detect and clean it.
            let fresh_git = GitService::new();
            let found = fresh_git.check_worktrees(&wt_base);
            assert_eq!(found.directories.len(), 1);
            assert_eq!(found.directories[0], wt_path);

            let clean = fresh_git.clean_worktrees(&wt_base).await;
            assert_eq!(clean.removed, 1);
            assert!(clean.errors.is_empty());
            assert!(!wt_path.exists());
        });

        let _ = std::fs::remove_dir_all(&root_tmp);
    }
}
