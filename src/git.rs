use anyhow::{anyhow, Result};
use dashmap::mapref::entry::Entry;
use dashmap::DashMap;
use shell_escape::escape;
use std::borrow::Cow;
use std::path::{Path, PathBuf};

use crate::config::settings;
use crate::ssh;

/// Escape a string for safe use in shell commands
fn shell_escape(s: &str) -> Cow<'_, str> {
    escape(Cow::Borrowed(s))
}

/// Parsed GitHub repository reference
#[derive(Debug, Clone)]
pub struct RepoRef {
    pub org: String,
    pub repo: String,
    pub branch: Option<String>,
    pub worktree: Option<WorktreeMode>,
}

/// Worktree creation mode
#[derive(Debug, Clone)]
pub enum WorktreeMode {
    /// Auto-generate worktree name from session ID
    Auto,
    /// User-specified worktree name
    Named(String),
}

impl RepoRef {
    /// Parse a repo reference string like "org/repo@branch --worktree=name"
    pub fn parse(input: &str) -> Option<Self> {
        let input = input.trim();

        // Check for worktree flag
        let (repo_part, worktree) = if let Some(idx) = input.find("--worktree") {
            let repo_part = input[..idx].trim();
            let worktree_part = input[idx..].trim();

            let worktree = if worktree_part.starts_with("--worktree=") {
                let name = worktree_part.strip_prefix("--worktree=")?.trim();
                if name.is_empty() {
                    return None;
                }
                Some(WorktreeMode::Named(name.to_string()))
            } else if worktree_part == "--worktree" {
                Some(WorktreeMode::Auto)
            } else {
                return None;
            };
            (repo_part, worktree)
        } else {
            (input, None)
        };

        // Parse org/repo@branch
        let (org_repo, branch) = if let Some((org_repo, branch)) = repo_part.split_once('@') {
            (org_repo, Some(branch.to_string()))
        } else {
            (repo_part, None)
        };

        // Parse org/repo
        let (org, repo) = org_repo.split_once('/')?;

        // Validate: must be exactly org/repo format (no additional slashes)
        if repo.contains('/') || org.is_empty() || repo.is_empty() {
            return None;
        }

        Some(RepoRef {
            org: org.to_string(),
            repo: repo.to_string(),
            branch,
            worktree,
        })
    }

    /// Get the full repo identifier (org/repo)
    pub fn full_name(&self) -> String {
        format!("{}/{}", self.org, self.repo)
    }

    /// Check if this looks like a GitHub repo reference (contains exactly one /)
    pub fn looks_like_repo(input: &str) -> bool {
        let input = input.split_whitespace().next().unwrap_or(input);
        let input = input.split('@').next().unwrap_or(input);
        let slash_count = input.chars().filter(|c| *c == '/').count();
        slash_count == 1 && !input.starts_with('/') && !input.ends_with('/')
    }
}

/// Manages GitHub repository cloning and worktree operations on the VM
pub struct GitManager {
    /// Tracks which repos have active sessions (repo full_name -> session_id)
    active_repos: DashMap<String, String>,
}

impl GitManager {
    pub fn new() -> Self {
        Self {
            active_repos: DashMap::new(),
        }
    }

    /// Run an SSH command on the VM
    async fn ssh_command(&self, cmd: &str) -> Result<String> {
        ssh::run_command(cmd).await
    }

    /// Get the path to the main clone of a repo
    fn repo_path(&self, repo_ref: &RepoRef) -> PathBuf {
        let s = settings();
        PathBuf::from(&s.repos_base_path)
            .join("github.com")
            .join(&repo_ref.org)
            .join(&repo_ref.repo)
    }

    /// Generate a worktree path
    fn worktree_path(&self, _repo_ref: &RepoRef, name: &str) -> PathBuf {
        let s = settings();
        PathBuf::from(&s.worktrees_path).join(name)
    }

    /// Ensure a repository is cloned on the VM, return path to main clone
    pub async fn ensure_repo(&self, repo_ref: &RepoRef) -> Result<PathBuf> {
        let repo_path = self.repo_path(repo_ref);
        let repo_path_str = repo_path.to_string_lossy();

        // Check if repo already exists
        let check_cmd = format!("test -d {} && echo exists", shell_escape(&repo_path_str));
        let exists = self.ssh_command(&check_cmd).await.map(|s| s.contains("exists")).unwrap_or(false);

        if exists {
            // Optionally pull if auto_pull is enabled
            let s = settings();
            if s.auto_pull {
                let pull_cmd = format!(
                    "cd {} && git pull --ff-only 2>/dev/null || true",
                    shell_escape(&repo_path_str)
                );
                let _ = self.ssh_command(&pull_cmd).await;
            }
        } else {
            // Clone the repo
            let parent_dir = repo_path.parent().ok_or_else(|| anyhow!("Invalid repo path"))?;
            let parent_str = parent_dir.to_string_lossy();

            // Create parent directory
            let mkdir_cmd = format!("mkdir -p {}", shell_escape(&parent_str));
            self.ssh_command(&mkdir_cmd).await?;

            // Clone with GH_TOKEN for private repo support
            // Format: https://{token}@github.com/org/repo.git
            let clone_url = format!(
                "https://\\$GH_TOKEN@github.com/{}/{}.git",
                shell_escape(&repo_ref.org),
                shell_escape(&repo_ref.repo)
            );
            let clone_cmd = format!(
                "git clone {} {}",
                clone_url,
                shell_escape(&repo_path_str)
            );
            self.ssh_command(&clone_cmd).await.map_err(|e| {
                anyhow!("Failed to clone repository: {}. Make sure GH_TOKEN is set for private repos.", e)
            })?;
        }

        Ok(repo_path)
    }

    /// Create a worktree for a session, return path to worktree
    pub async fn create_worktree(
        &self,
        repo_ref: &RepoRef,
        session_id: &str,
    ) -> Result<PathBuf> {
        // Ensure the base repo exists first
        let repo_path = self.ensure_repo(repo_ref).await?;
        let repo_path_str = repo_path.to_string_lossy();

        // Generate worktree name
        let worktree_name = match &repo_ref.worktree {
            Some(WorktreeMode::Named(name)) => name.clone(),
            Some(WorktreeMode::Auto) | None => {
                format!("{}-{}", repo_ref.repo, &session_id[..8])
            }
        };

        let worktree_path = self.worktree_path(repo_ref, &worktree_name);
        let worktree_path_str = worktree_path.to_string_lossy();

        // Ensure worktrees directory exists
        let s = settings();
        let mkdir_cmd = format!("mkdir -p {}", shell_escape(&s.worktrees_path));
        self.ssh_command(&mkdir_cmd).await?;

        // Create worktree
        let worktree_cmd = if let Some(branch) = &repo_ref.branch {
            format!(
                "git -C {} worktree add {} {}",
                shell_escape(&repo_path_str),
                shell_escape(&worktree_path_str),
                shell_escape(branch)
            )
        } else {
            format!(
                "git -C {} worktree add {}",
                shell_escape(&repo_path_str),
                shell_escape(&worktree_path_str)
            )
        };

        self.ssh_command(&worktree_cmd).await.map_err(|e| {
            anyhow!("Failed to create worktree: {}", e)
        })?;

        Ok(worktree_path)
    }

    /// Check if the main clone of a repo has an active session
    #[allow(dead_code)]
    pub fn is_repo_in_use(&self, repo_ref: &RepoRef) -> Option<String> {
        self.active_repos.get(&repo_ref.full_name()).map(|r| r.clone())
    }

    /// Mark a repo's main clone as in use by a session
    #[allow(dead_code)]
    pub fn mark_repo_in_use(&self, repo_ref: &RepoRef, session_id: &str) {
        self.active_repos.insert(repo_ref.full_name(), session_id.to_string());
    }

    /// Atomically try to acquire a repo for a session
    /// Returns true if acquired, false if already in use
    /// This prevents race conditions between is_repo_in_use and mark_repo_in_use
    pub fn try_acquire_repo(&self, repo_ref: &RepoRef, session_id: &str) -> Result<(), String> {
        match self.active_repos.entry(repo_ref.full_name()) {
            Entry::Occupied(e) => Err(e.get().clone()),
            Entry::Vacant(v) => {
                v.insert(session_id.to_string());
                Ok(())
            }
        }
    }

    /// Release a repo's main clone
    #[allow(dead_code)]
    pub fn release_repo(&self, repo_ref: &RepoRef) {
        self.active_repos.remove(&repo_ref.full_name());
    }

    /// Release a repo by session ID (useful when we don't have the RepoRef)
    pub fn release_repo_by_session(&self, session_id: &str) {
        self.active_repos.retain(|_, v| v != session_id);
    }

    /// Cleanup a worktree (optional - not called automatically)
    #[allow(dead_code)]
    pub async fn cleanup_worktree(&self, repo_ref: &RepoRef, worktree_path: &Path) -> Result<()> {
        let repo_path = self.repo_path(repo_ref);
        let repo_path_str = repo_path.to_string_lossy();
        let worktree_path_str = worktree_path.to_string_lossy();

        // Remove worktree from git
        let remove_cmd = format!(
            "git -C {} worktree remove {} --force",
            shell_escape(&repo_path_str),
            shell_escape(&worktree_path_str)
        );
        self.ssh_command(&remove_cmd).await?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_repo() {
        let ref_ = RepoRef::parse("org/repo").unwrap();
        assert_eq!(ref_.org, "org");
        assert_eq!(ref_.repo, "repo");
        assert!(ref_.branch.is_none());
        assert!(ref_.worktree.is_none());
    }

    #[test]
    fn test_parse_repo_with_branch() {
        let ref_ = RepoRef::parse("org/repo@main").unwrap();
        assert_eq!(ref_.org, "org");
        assert_eq!(ref_.repo, "repo");
        assert_eq!(ref_.branch, Some("main".to_string()));
        assert!(ref_.worktree.is_none());
    }

    #[test]
    fn test_parse_repo_with_worktree() {
        let ref_ = RepoRef::parse("org/repo --worktree").unwrap();
        assert_eq!(ref_.org, "org");
        assert_eq!(ref_.repo, "repo");
        assert!(ref_.branch.is_none());
        assert!(matches!(ref_.worktree, Some(WorktreeMode::Auto)));
    }

    #[test]
    fn test_parse_repo_with_named_worktree() {
        let ref_ = RepoRef::parse("org/repo --worktree=my-worktree").unwrap();
        assert_eq!(ref_.org, "org");
        assert_eq!(ref_.repo, "repo");
        assert!(matches!(ref_.worktree, Some(WorktreeMode::Named(ref n)) if n == "my-worktree"));
    }

    #[test]
    fn test_parse_full_syntax() {
        let ref_ = RepoRef::parse("org/repo@feature-branch --worktree=feature-wt").unwrap();
        assert_eq!(ref_.org, "org");
        assert_eq!(ref_.repo, "repo");
        assert_eq!(ref_.branch, Some("feature-branch".to_string()));
        assert!(matches!(ref_.worktree, Some(WorktreeMode::Named(ref n)) if n == "feature-wt"));
    }

    #[test]
    fn test_parse_invalid_no_slash() {
        assert!(RepoRef::parse("repo").is_none());
    }

    #[test]
    fn test_parse_invalid_multiple_slashes() {
        assert!(RepoRef::parse("org/sub/repo").is_none());
    }

    #[test]
    fn test_looks_like_repo() {
        assert!(RepoRef::looks_like_repo("org/repo"));
        assert!(RepoRef::looks_like_repo("org/repo@branch"));
        assert!(RepoRef::looks_like_repo("org/repo --worktree"));
        assert!(!RepoRef::looks_like_repo("myproject"));
        assert!(!RepoRef::looks_like_repo("/absolute/path"));
        assert!(!RepoRef::looks_like_repo("org/sub/repo"));
    }

    #[test]
    fn test_full_name() {
        let ref_ = RepoRef::parse("myorg/myrepo@branch").unwrap();
        assert_eq!(ref_.full_name(), "myorg/myrepo");
    }
}
