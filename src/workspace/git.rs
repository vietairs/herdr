use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitSpaceMetadata {
    pub key: String,
    pub checkout_key: String,
    pub label: String,
    pub repo_root: PathBuf,
    pub is_linked_worktree: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitWorktreeInfo {
    pub repo_root: PathBuf,
    pub git_dir: PathBuf,
    pub git_common_dir: PathBuf,
    pub is_bare: bool,
    pub is_linked_worktree: bool,
}

pub fn derive_label_from_cwd(cwd: &Path) -> String {
    if let Some(repo_root) = git_repo_root(cwd) {
        if let Some(name) = repo_root.file_name().and_then(|n| n.to_str()) {
            return name.to_string();
        }
    }

    if let Ok(home) = std::env::var("HOME") {
        let home = Path::new(&home);
        if cwd == home {
            return "~".to_string();
        }
    }

    cwd.file_name()
        .and_then(|n| n.to_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(|| cwd.display().to_string())
}

pub fn git_worktree_info(cwd: &Path) -> Option<GitWorktreeInfo> {
    let repo_root = git_rev_parse(cwd, &["--show-toplevel"])?;
    let git_dir = git_rev_parse(cwd, &["--path-format=absolute", "--git-dir"])?;
    let git_common_dir = git_rev_parse(cwd, &["--path-format=absolute", "--git-common-dir"])?;
    let is_bare = git_rev_parse(cwd, &["--is-bare-repository"])? == "true";
    let is_linked_worktree =
        canonicalize_best_effort(&git_dir) != canonicalize_best_effort(&git_common_dir);

    Some(GitWorktreeInfo {
        repo_root: PathBuf::from(repo_root),
        git_dir: PathBuf::from(git_dir),
        git_common_dir: PathBuf::from(git_common_dir),
        is_bare,
        is_linked_worktree,
    })
}

fn git_rev_parse(cwd: &Path, args: &[&str]) -> Option<String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .arg("rev-parse")
        .args(args)
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8(output.stdout).ok()?;
    Some(stdout.trim().to_string())
}

fn canonicalize_best_effort(path: &str) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| PathBuf::from(path))
}

pub fn git_space_metadata(cwd: &Path) -> Option<GitSpaceMetadata> {
    let info = git_worktree_info(cwd)?;
    if info.is_bare {
        return None;
    }
    let key = canonicalize_best_effort_path(&info.git_common_dir)
        .display()
        .to_string();
    let checkout_key = canonicalize_best_effort_path(&info.repo_root)
        .display()
        .to_string();
    let label_path = if info
        .git_common_dir
        .file_name()
        .and_then(|name| name.to_str())
        == Some(".git")
    {
        info.git_common_dir.parent().unwrap_or(&info.repo_root)
    } else {
        &info.repo_root
    };
    let label = label_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("repo")
        .to_string();
    Some(GitSpaceMetadata {
        key,
        checkout_key,
        label,
        repo_root: info.repo_root,
        is_linked_worktree: info.is_linked_worktree,
    })
}

fn canonicalize_best_effort_path(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

pub fn git_branch(cwd: &Path) -> Option<String> {
    let repo_root = git_repo_root(cwd)?;
    let git_dir = git_dir_for_repo_root(&repo_root)?;
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    parse_git_head_branch(&head)
}

fn git_dir_for_repo_root(repo_root: &Path) -> Option<PathBuf> {
    let git_path = repo_root.join(".git");
    if git_path.is_dir() {
        return Some(git_path);
    }

    let gitdir = std::fs::read_to_string(&git_path).ok()?;
    let relative = gitdir.trim().strip_prefix("gitdir:")?.trim();
    let resolved = Path::new(relative);
    Some(if resolved.is_absolute() {
        resolved.to_path_buf()
    } else {
        repo_root.join(resolved)
    })
}

fn parse_git_head_branch(head: &str) -> Option<String> {
    let branch = head.trim().strip_prefix("ref: refs/heads/")?;
    (!branch.is_empty()).then(|| branch.to_string())
}

fn git_repo_root(start: &Path) -> Option<PathBuf> {
    let mut current = if start.is_dir() {
        start.to_path_buf()
    } else {
        start.parent()?.to_path_buf()
    };

    loop {
        if current.join(".git").exists() {
            return Some(current);
        }
        if !current.pop() {
            return None;
        }
    }
}

pub(super) fn git_ahead_behind(cwd: &Path) -> Option<(usize, usize)> {
    git_repo_root(cwd)?;

    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["rev-list", "--left-right", "--count", "HEAD...@{upstream}"])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8(output.stdout).ok()?;
    parse_git_ahead_behind_output(&stdout)
}

fn parse_git_ahead_behind_output(stdout: &str) -> Option<(usize, usize)> {
    let mut parts = stdout.split_whitespace();
    let ahead = parts.next()?.parse().ok()?;
    let behind = parts.next()?.parse().ok()?;
    Some((ahead, behind))
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    fn temp_test_dir(name: &str) -> PathBuf {
        let unique = format!(
            "herdr-workspace-tests-{}-{}-{}",
            name,
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let path = std::env::temp_dir().join(unique);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn git_branch_reads_head_from_standard_repo() {
        let root = temp_test_dir("standard-repo");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();

        assert_eq!(git_branch(&root).as_deref(), Some("main"));

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn git_branch_reads_head_from_worktree_gitdir_file() {
        let root = temp_test_dir("worktree");
        let worktree_git_dir = root.join(".bare/worktrees/feature");
        std::fs::create_dir_all(&worktree_git_dir).unwrap();
        std::fs::write(root.join(".git"), "gitdir: .bare/worktrees/feature\n").unwrap();
        std::fs::write(worktree_git_dir.join("HEAD"), "ref: refs/heads/feature\n").unwrap();

        assert_eq!(git_branch(&root).as_deref(), Some("feature"));

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn git_branch_returns_none_for_detached_head() {
        let root = temp_test_dir("detached-head");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join(".git/HEAD"), "3e1b9a8d\n").unwrap();

        assert_eq!(git_branch(&root), None);

        std::fs::remove_dir_all(root).unwrap();
    }
}
