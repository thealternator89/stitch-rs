use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::infrastructure::git::GitService;
use crate::infrastructure::providers::CodeReviewProvider;
use crate::types::{AppSettings, PrDiffFile};

/// Normalized repository path resolved from a user-selected directory or subdirectory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedRepoPath {
    pub path: PathBuf,
    pub was_modified: bool,
}

/// Output of checking out a PR branch or detached worktree and computing its changed files.
#[derive(Debug, Clone)]
pub struct CheckoutResult {
    pub commit_sha: String,
    pub effective_repo_path: PathBuf,
    pub diff_files: Vec<PrDiffFile>,
    pub using_worktree: bool,
}

#[derive(Debug, Clone)]
pub enum CheckoutError {
    DirtyRepo(String),
    Other(String),
}

impl std::fmt::Display for CheckoutError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DirtyRepo(msg) | Self::Other(msg) => f.write_str(msg),
        }
    }
}

/// Verifies `selected` is inside a Git repository and normalizes it to the repository top-level root.
pub async fn verify_repo_path(git: &GitService, selected: &Path) -> VerifiedRepoPath {
    if git.check_git_repo(selected).await {
        if let Ok(root) = git.get_repo_root(selected).await {
            let differs = match (
                std::fs::canonicalize(selected),
                std::fs::canonicalize(&root),
            ) {
                (Ok(canon_sel), Ok(canon_root)) => canon_sel != canon_root,
                _ => selected != root,
            };
            return VerifiedRepoPath {
                path: root,
                was_modified: differs,
            };
        }
    }
    VerifiedRepoPath {
        path: selected.to_path_buf(),
        was_modified: false,
    }
}

fn canonical_or_raw(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Manages PR repository checkout, worktree isolation, and ref restoration.
pub struct PrReviewerService {
    git: Arc<GitService>,
    active_checkouts: Mutex<HashMap<PathBuf, String>>,
    active_worktrees: Mutex<HashMap<PathBuf, PathBuf>>,
}

impl PrReviewerService {
    pub fn new(git: Arc<GitService>) -> Self {
        Self {
            git,
            active_checkouts: Mutex::new(HashMap::new()),
            active_worktrees: Mutex::new(HashMap::new()),
        }
    }

    pub async fn checkout_pr(
        &self,
        repo_path: &Path,
        pr_number: u64,
        target_branch: &str,
        expected_repo_name: Option<&str>,
        code_provider: Option<&dyn CodeReviewProvider>,
        settings: &AppSettings,
    ) -> Result<CheckoutResult, CheckoutError> {
        if !self.git.check_git_repo(repo_path).await {
            return Err(CheckoutError::Other(
                "The selected path is not a valid Git repository.".to_string(),
            ));
        }

        let repo_root = self
            .git
            .get_repo_root(repo_path)
            .await
            .unwrap_or_else(|_| repo_path.to_path_buf());
        let repo_key = canonical_or_raw(&repo_root);

        if let Some(expected) = expected_repo_name.map(str::trim).filter(|s| !s.is_empty()) {
            if let (Some(provider), Some(remote_url)) =
                (code_provider, self.git.get_remote_url(&repo_root).await)
            {
                if let Some(parsed_remote) = provider.parse_remote_url(&remote_url) {
                    if !parsed_remote.repo_name.eq_ignore_ascii_case(expected) {
                        return Err(CheckoutError::Other(format!(
                            "The selected repository \"{}\" does not match the Pull Request repository \"{}\".",
                            parsed_remote.repo_name, expected
                        )));
                    }
                }
            }
        }

        // Restore any previously checked-out ref or remove any stale worktree for this repo first.
        if let Err(err) = self.cleanup_checkout(&repo_root).await {
            eprintln!("Warning: cleanup before PR checkout failed: {err}");
        }

        if settings.git_worktree_enabled {
            if let Some(base_dir) = settings.git_worktree_base_dir.clone() {
                if !base_dir.exists() {
                    std::fs::create_dir_all(&base_dir).map_err(|err| {
                        CheckoutError::Other(format!(
                            "Failed to create Git worktree base directory {}: {err}",
                            base_dir.display()
                        ))
                    })?;
                }

                let commit_sha = self
                    .git
                    .fetch_pr(&repo_root, pr_number)
                    .await
                    .map_err(|err| CheckoutError::Other(err.to_string()))?;

                let repo_slug = expected_repo_name
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(|name| GitService::sanitize_name(name, "repo"))
                    .unwrap_or_else(|| GitService::sanitize_repo_name(&repo_root));

                let worktree_name = format!("{repo_slug}_pr_{pr_number}");
                let worktree_path = base_dir.join(worktree_name);

                self.git
                    .add_worktree(&repo_root, &worktree_path, &commit_sha)
                    .await
                    .map_err(|err| CheckoutError::Other(err.to_string()))?;

                {
                    let mut map = self.active_worktrees.lock().await;
                    map.insert(repo_key, worktree_path.clone());
                }

                let diff_files = self
                    .git
                    .get_diff_files(&worktree_path, target_branch)
                    .await
                    .map_err(|err| CheckoutError::Other(err.to_string()))?;

                return Ok(CheckoutResult {
                    commit_sha,
                    effective_repo_path: worktree_path,
                    diff_files,
                    using_worktree: true,
                });
            }
        }

        if self.git.has_uncommitted_changes(&repo_root).await {
            return Err(CheckoutError::DirtyRepo(
                "The local git repository has uncommitted changes. Please commit, stash, or revert them first."
                    .to_string(),
            ));
        }

        let original_ref = self
            .git
            .get_current_ref(&repo_root)
            .await
            .map_err(|err| CheckoutError::Other(err.to_string()))?;

        let commit_sha = self
            .git
            .fetch_and_checkout_pr(&repo_root, pr_number)
            .await
            .map_err(|err| CheckoutError::Other(err.to_string()))?;

        {
            let mut map = self.active_checkouts.lock().await;
            map.insert(repo_key, original_ref);
        }

        let diff_files = self
            .git
            .get_diff_files(&repo_root, target_branch)
            .await
            .map_err(|err| CheckoutError::Other(err.to_string()))?;

        Ok(CheckoutResult {
            commit_sha,
            effective_repo_path: repo_root,
            diff_files,
            using_worktree: false,
        })
    }

    pub async fn cleanup_checkout(&self, repo_path: &Path) -> anyhow::Result<()> {
        let repo_root = self
            .git
            .get_repo_root(repo_path)
            .await
            .unwrap_or_else(|_| repo_path.to_path_buf());
        let repo_key = canonical_or_raw(&repo_root);

        let wt = {
            let mut map = self.active_worktrees.lock().await;
            map.remove(&repo_key)
                .or_else(|| map.remove(repo_path))
                .or_else(|| map.remove(&repo_root))
        };
        if let Some(worktree_path) = wt {
            self.git
                .remove_worktree(Some(&repo_root), &worktree_path)
                .await;
        }

        let prev_ref = {
            let mut map = self.active_checkouts.lock().await;
            map.remove(&repo_key)
                .or_else(|| map.remove(repo_path))
                .or_else(|| map.remove(&repo_root))
        };
        if let Some(original_ref) = prev_ref {
            if let Err(err) = self.git.restore_ref(&repo_root, &original_ref).await {
                // Re-insert so the user can retry restoring once changes are cleaned up.
                let mut map = self.active_checkouts.lock().await;
                map.insert(repo_key, original_ref);
                return Err(err);
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command as StdCommand;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn verify_repo_path_resolves_subdirectory_to_git_root() {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        let tmp = std::env::temp_dir().join(format!("stitch-rs-verify-repo-{nanos}"));
        let sub = tmp.join("packages").join("app");
        std::fs::create_dir_all(&sub).expect("create sub");

        let out = StdCommand::new("git")
            .current_dir(&tmp)
            .arg("init")
            .output()
            .expect("git init");
        assert!(out.status.success());

        crate::infrastructure::runtime::runtime().block_on(async {
            let git = GitService::new();
            let verified_sub = verify_repo_path(&git, &sub).await;
            assert!(verified_sub.was_modified);
            assert_eq!(
                canonical_or_raw(&verified_sub.path),
                canonical_or_raw(&tmp)
            );

            let verified_root = verify_repo_path(&git, &verified_sub.path).await;
            assert!(!verified_root.was_modified);
        });

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
