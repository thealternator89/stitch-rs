use std::collections::HashSet;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::Mutex;

use anyhow::Context as _;
use tokio::process::Command;

use crate::types::{PrDiffFile, PrDiffStatus, WorktreeCheckResult, WorktreeCleanResult};

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

    /// Reads `remote.origin.url` from the Git repository at `repo_path`.
    pub async fn get_remote_url(&self, repo_path: &Path) -> Option<String> {
        let out = Self::run_git(repo_path, &["config", "--get", "remote.origin.url"])
            .await
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let trimmed = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        }
    }

    /// Returns `true` when `git status --porcelain` reports any uncommitted changes.
    pub async fn has_uncommitted_changes(&self, repo_path: &Path) -> bool {
        match Self::run_git(repo_path, &["status", "--porcelain"]).await {
            Ok(out) if out.status.success() => {
                !String::from_utf8_lossy(&out.stdout).trim().is_empty()
            }
            _ => false,
        }
    }

    /// Resolves the current checked-out branch name, falling back to the commit SHA when detached.
    pub async fn get_current_ref(&self, repo_path: &Path) -> anyhow::Result<String> {
        if let Ok(branch_out) = Self::run_git(repo_path, &["symbolic-ref", "--short", "-q", "HEAD"]).await {
            if branch_out.status.success() {
                let branch = String::from_utf8_lossy(&branch_out.stdout).trim().to_string();
                if !branch.is_empty() {
                    return Ok(branch);
                }
            }
        }

        let sha_out = Self::run_git(repo_path, &["rev-parse", "HEAD"]).await?;
        if !sha_out.status.success() {
            let stderr = String::from_utf8_lossy(&sha_out.stderr).trim().to_string();
            anyhow::bail!("Failed to determine current Git ref: {stderr}");
        }
        let sha = String::from_utf8_lossy(&sha_out.stdout).trim().to_string();
        if sha.is_empty() {
            anyhow::bail!("git rev-parse HEAD returned empty SHA");
        }
        Ok(sha)
    }

    /// Restores the repository to `ref_name`, refusing if uncommitted changes are present.
    pub async fn restore_ref(&self, repo_path: &Path, ref_name: &str) -> anyhow::Result<()> {
        if self.has_uncommitted_changes(repo_path).await {
            anyhow::bail!("Cannot restore repository because it has uncommitted changes.");
        }
        let out = Self::run_git(repo_path, &["checkout", ref_name]).await?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            anyhow::bail!("Failed to restore Git ref '{ref_name}': {stderr}");
        }
        Ok(())
    }

    /// Fetches `refs/pull/{pr_number}/merge` (falling back to `/head`) and returns `FETCH_HEAD` SHA.
    pub async fn fetch_pr(&self, repo_path: &Path, pr_number: u64) -> anyhow::Result<String> {
        let merge_ref = format!("refs/pull/{pr_number}/merge");
        let head_ref = format!("refs/pull/{pr_number}/head");

        let primary = Self::run_git(repo_path, &["fetch", "origin", &merge_ref]).await;
        let fetched_ok = match &primary {
            Ok(out) if out.status.success() => true,
            _ => {
                let fallback = Self::run_git(repo_path, &["fetch", "origin", &head_ref]).await?;
                if !fallback.status.success() {
                    let err1 = primary
                        .as_ref()
                        .map(|o| String::from_utf8_lossy(&o.stderr).trim().to_string())
                        .unwrap_or_default();
                    let err2 = String::from_utf8_lossy(&fallback.stderr).trim().to_string();
                    anyhow::bail!(
                        "Failed to fetch PR #{pr_number} from origin: {err1} (fallback: {err2})"
                    );
                }
                true
            }
        };

        if fetched_ok {
            let sha_out = Self::run_git(repo_path, &["rev-parse", "FETCH_HEAD"]).await?;
            if sha_out.status.success() {
                let sha = String::from_utf8_lossy(&sha_out.stdout).trim().to_string();
                if !sha.is_empty() {
                    return Ok(sha);
                }
            }
        }

        anyhow::bail!("Fetched PR #{pr_number} but failed to resolve FETCH_HEAD SHA.");
    }

    /// Fetches PR refs and checks out `FETCH_HEAD` directly in `repo_path`, returning the checked-out commit SHA.
    pub async fn fetch_and_checkout_pr(
        &self,
        repo_path: &Path,
        pr_number: u64,
    ) -> anyhow::Result<String> {
        let _fetch_sha = self.fetch_pr(repo_path, pr_number).await?;
        let checkout = Self::run_git(repo_path, &["checkout", "FETCH_HEAD"]).await?;
        if !checkout.status.success() {
            let stderr = String::from_utf8_lossy(&checkout.stderr).trim().to_string();
            anyhow::bail!("Failed to checkout PR #{pr_number}: {stderr}");
        }
        let head_out = Self::run_git(repo_path, &["rev-parse", "HEAD"]).await?;
        if !head_out.status.success() {
            let stderr = String::from_utf8_lossy(&head_out.stderr).trim().to_string();
            anyhow::bail!("Failed to read HEAD commit SHA after checkout: {stderr}");
        }
        Ok(String::from_utf8_lossy(&head_out.stdout).trim().to_string())
    }

    /// Detects merge commits (`> 1` parent) to diff against `HEAD~1`; otherwise uses `target_branch`.
    pub async fn get_diff_target(&self, repo_path: &Path, target_branch: &str) -> String {
        if let Ok(out) = Self::run_git(repo_path, &["log", "-1", "--format=%P", "HEAD"]).await {
            if out.status.success() {
                let count = String::from_utf8_lossy(&out.stdout)
                    .split_whitespace()
                    .count();
                if count > 1 {
                    return "HEAD~1".to_string();
                }
            }
        }
        target_branch.to_string()
    }

    /// Parses output from `git diff --name-status`.
    pub fn parse_diff_name_status(output: &str) -> Vec<PrDiffFile> {
        let mut files = Vec::new();
        for line in output.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let parts: Vec<&str> = line.split('\t').collect();
            if parts.len() < 2 {
                continue;
            }
            let raw_status = parts[0].trim();
            let status = if raw_status.starts_with('A') {
                PrDiffStatus::Added
            } else if raw_status.starts_with('D') {
                PrDiffStatus::Deleted
            } else if raw_status.starts_with('M') {
                PrDiffStatus::Modified
            } else if raw_status.starts_with('R') {
                PrDiffStatus::Renamed
            } else if raw_status.starts_with('T') {
                PrDiffStatus::TypeChanged
            } else {
                PrDiffStatus::Unknown
            };
            let path = if status == PrDiffStatus::Renamed && parts.len() >= 3 {
                parts[2].trim().to_string()
            } else {
                parts[1].trim().to_string()
            };
            if !path.is_empty() {
                files.push(PrDiffFile { path, status });
            }
        }
        files
    }

    /// Lists changed files in the current PR checkout against `target_branch` (or `HEAD~1` for merge commits).
    pub async fn get_diff_files(
        &self,
        repo_path: &Path,
        target_branch: &str,
    ) -> anyhow::Result<Vec<PrDiffFile>> {
        let diff_target = self.get_diff_target(repo_path, target_branch).await;
        let range = format!("{diff_target}...HEAD");
        let out = Self::run_git(repo_path, &["diff", "--name-status", &range]).await?;
        if !out.status.success() {
            // Fallback to two-dot diff when three-dot merge-base fails on shallow clones or orphan branches.
            let two_dot = format!("{diff_target}..HEAD");
            let fallback = Self::run_git(repo_path, &["diff", "--name-status", &two_dot]).await?;
            if !fallback.status.success() {
                let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
                anyhow::bail!("Failed to get diff files against '{diff_target}': {stderr}");
            }
            return Ok(Self::parse_diff_name_status(&String::from_utf8_lossy(
                &fallback.stdout,
            )));
        }
        Ok(Self::parse_diff_name_status(&String::from_utf8_lossy(
            &out.stdout,
        )))
    }

    /// Sanitizes an identifier so only ASCII alphanumerics, `-`, and `_` remain.
    pub fn sanitize_name(raw: &str, fallback: &str) -> String {
        let sanitized: String = raw
            .trim()
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
            fallback.to_string()
        } else {
            sanitized
        }
    }

    /// Sanitizes a repository directory name for safe use inside worktree directory names.
    pub fn sanitize_repo_name(repo_root: &Path) -> String {
        let raw = repo_root
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("repo");
        Self::sanitize_name(raw, "repo")
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

    #[test]
    fn parse_diff_name_status_maps_all_git_statuses_and_renames() {
        let raw = "\
A\tsrc/new_file.rs
M\tsrc/main.rs
D\tsrc/removed.rs
R100\tsrc/old_name.rs\tsrc/new_name.rs
T\tsrc/symlink_or_mode.rs
";
        let files = GitService::parse_diff_name_status(raw);
        assert_eq!(
            files,
            vec![
                PrDiffFile {
                    path: "src/new_file.rs".to_string(),
                    status: PrDiffStatus::Added,
                },
                PrDiffFile {
                    path: "src/main.rs".to_string(),
                    status: PrDiffStatus::Modified,
                },
                PrDiffFile {
                    path: "src/removed.rs".to_string(),
                    status: PrDiffStatus::Deleted,
                },
                PrDiffFile {
                    path: "src/new_name.rs".to_string(),
                    status: PrDiffStatus::Renamed,
                },
                PrDiffFile {
                    path: "src/symlink_or_mode.rs".to_string(),
                    status: PrDiffStatus::TypeChanged,
                },
            ]
        );
    }

    #[test]
    fn git_pr_helpers_check_dirty_ref_restore_and_diff_files() {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_nanos();
        let root_tmp = std::env::temp_dir().join(format!("stitch-rs-pr-git-test-{nanos}"));
        std::fs::create_dir_all(&root_tmp).expect("create repo dir");

        run_std_git(&root_tmp, &["init"]);
        run_std_git(&root_tmp, &["config", "user.name", "Stitch Test"]);
        run_std_git(&root_tmp, &["config", "user.email", "test@stitch.local"]);
        std::fs::write(root_tmp.join("base.txt"), "base").expect("write base");
        run_std_git(&root_tmp, &["add", "."]);
        run_std_git(&root_tmp, &["commit", "-m", "base"]);
        run_std_git(&root_tmp, &["branch", "-M", "main"]);

        run_std_git(&root_tmp, &["checkout", "-b", "feature/test"]);
        std::fs::write(root_tmp.join("feature.txt"), "feat").expect("write feature");
        run_std_git(&root_tmp, &["add", "."]);
        run_std_git(&root_tmp, &["commit", "-m", "feat"]);

        let rt = crate::infrastructure::runtime::runtime();
        rt.block_on(async {
            let git = GitService::new();
            assert!(!git.has_uncommitted_changes(&root_tmp).await);
            let cur = git.get_current_ref(&root_tmp).await.expect("current ref");
            assert_eq!(cur, "feature/test");

            let diff = git.get_diff_files(&root_tmp, "main").await.expect("diff files");
            assert_eq!(
                diff,
                vec![PrDiffFile {
                    path: "feature.txt".to_string(),
                    status: PrDiffStatus::Added,
                }]
            );

            // Dirty repo must refuse restore_ref.
            std::fs::write(root_tmp.join("dirty.txt"), "uncommitted").expect("dirty file");
            assert!(git.has_uncommitted_changes(&root_tmp).await);
            assert!(git.restore_ref(&root_tmp, "main").await.is_err());

            let _ = std::fs::remove_file(root_tmp.join("dirty.txt"));
            git.restore_ref(&root_tmp, "main").await.expect("restore main");
            assert_eq!(
                git.get_current_ref(&root_tmp).await.expect("restored ref"),
                "main"
            );
        });

        let _ = std::fs::remove_dir_all(&root_tmp);
    }
}
