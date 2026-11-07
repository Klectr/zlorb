use git2::Repository;

use crate::error::ZlorbError;
use crate::log::Logger;
use std::path::{Path, PathBuf};

/// Returns true if the given path opens as a git repository.
pub fn is_git_repo(path: &str) -> bool {
    Repository::open(path).is_ok()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DetectedRepo {
    pub root: PathBuf,
    pub branch: String,
    pub remote: String,
}

pub fn detect_repo(path: &Path) -> Result<DetectedRepo, ZlorbError> {
    let repo = match Repository::open(path) {
        Ok(repo) => repo,
        Err(_) => Repository::discover(path).map_err(|_| {
            ZlorbError::Other(format!(
                "{} is not a valid git repository",
                path.display()
            ))
        })?,
    };
    let workdir = repo.workdir().ok_or_else(|| {
        ZlorbError::Other(format!(
            "{} is a bare git repository; zlorb can only watch a working tree",
            path.display()
        ))
    })?;
    let root: PathBuf = workdir.components().collect();
    let head = repo.head().map_err(ZlorbError::Git)?;
    if !head.is_branch() {
        return Err(ZlorbError::Other(format!(
            "{} has a detached HEAD; check out the branch zlorb should watch and add again",
            root.display()
        )));
    }
    let branch = head
        .shorthand()
        .ok_or_else(|| {
            ZlorbError::Other(format!(
                "could not read the current branch of {}",
                root.display()
            ))
        })?
        .to_string();
    let remotes = repo.remotes().map_err(ZlorbError::Git)?;
    let names: Vec<&str> = remotes.iter().flatten().collect();
    let remote = if names.contains(&"origin") {
        "origin".to_string()
    } else if names.len() == 1 {
        names[0].to_string()
    } else {
        "origin".to_string()
    };
    Ok(DetectedRepo {
        root,
        branch,
        remote,
    })
}

pub fn fetch_update(repo_path: String) {
    let _repo: Option<Repository> = match Repository::open(repo_path) {
        Ok(repo) => Some(repo),
        Err(e) => {
            Logger::error(format!("failed to init: {}", e));
            None
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use git2::{IndexAddOption, RepositoryInitOptions, Signature};
    use std::env;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_dir(label: &str) -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = env::temp_dir().join(format!(
            "zlorb-git-detect-test-{}-{}-{}",
            std::process::id(),
            label,
            n
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn init_repo(path: &Path, branch: &str) -> Repository {
        let mut opts = RepositoryInitOptions::new();
        opts.initial_head(branch);
        Repository::init_opts(path, &opts).unwrap()
    }

    fn commit_all(repo: &Repository, message: &str) -> git2::Oid {
        let workdir = repo.workdir().unwrap().to_path_buf();
        fs::write(workdir.join("file.txt"), message).unwrap();
        let mut index = repo.index().unwrap();
        index
            .add_all(["*"].iter(), IndexAddOption::DEFAULT, None)
            .unwrap();
        index.write().unwrap();
        let tree_oid = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_oid).unwrap();
        let sig = Signature::now("zlorb-test", "zlorb-test@example.com").unwrap();
        let parent = repo
            .head()
            .ok()
            .and_then(|head| head.target())
            .map(|oid| repo.find_commit(oid).unwrap());
        match parent {
            Some(parent) => repo
                .commit(Some("HEAD"), &sig, &sig, message, &tree, &[&parent])
                .unwrap(),
            None => repo
                .commit(Some("HEAD"), &sig, &sig, message, &tree, &[])
                .unwrap(),
        }
    }

    #[test]
    fn test_fetch_update_valid_repo() {
        // Initialize an empty repository
        let mut tmp_dir = env::temp_dir();
        tmp_dir.push("zlorb_git_test_valid");
        let _ = fs::remove_dir_all(&tmp_dir);
        fs::create_dir_all(&tmp_dir).unwrap();

        let _repo = Repository::init(&tmp_dir).unwrap();

        // Should load the repo and not error (no panic/return value to check right now)
        fetch_update(tmp_dir.to_str().unwrap().to_string());

        let _ = fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn test_fetch_update_invalid_repo() {
        // Directory that is not a git repository
        let mut tmp_dir = env::temp_dir();
        tmp_dir.push("zlorb_git_test_invalid");
        let _ = fs::remove_dir_all(&tmp_dir);
        fs::create_dir_all(&tmp_dir).unwrap();

        // Should log an error but not panic
        fetch_update(tmp_dir.to_str().unwrap().to_string());

        let _ = fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn detect_repo_reads_the_main_branch() {
        let root = temp_dir("main-branch");
        let repo = init_repo(&root, "main");
        commit_all(&repo, "v1");
        drop(repo);
        let detected = detect_repo(&root).unwrap();
        assert_eq!(detected.branch, "main");
        assert_eq!(detected.root, root);
        assert_eq!(detected.remote, "origin");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn detect_repo_reads_the_master_branch() {
        let root = temp_dir("master-branch");
        let repo = init_repo(&root, "master");
        commit_all(&repo, "v1");
        drop(repo);
        let detected = detect_repo(&root).unwrap();
        assert_eq!(detected.branch, "master");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn detect_repo_reads_the_checked_out_branch_not_the_initial_one() {
        let root = temp_dir("checked-out");
        let repo = init_repo(&root, "main");
        let oid = commit_all(&repo, "v1");
        let commit = repo.find_commit(oid).unwrap();
        repo.branch("dev", &commit, false).unwrap();
        drop(commit);
        repo.set_head("refs/heads/dev").unwrap();
        drop(repo);
        let detected = detect_repo(&root).unwrap();
        assert_eq!(detected.branch, "dev");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn detect_repo_from_a_subdirectory_finds_the_root() {
        let root = temp_dir("subdir");
        let repo = init_repo(&root, "main");
        commit_all(&repo, "v1");
        drop(repo);
        let subdir = root.join("nested").join("inner");
        fs::create_dir_all(&subdir).unwrap();
        let detected = detect_repo(&subdir).unwrap();
        assert_eq!(detected.root, root);
        assert_eq!(detected.branch, "main");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn detect_repo_at_a_nested_repo_root_returns_the_nested_repo() {
        let root = temp_dir("nested-root");
        let parent = init_repo(&root, "main");
        commit_all(&parent, "parent v1");
        drop(parent);
        let nested_path = root.join("vendor").join("child");
        fs::create_dir_all(nested_path.parent().unwrap()).unwrap();
        let nested = init_repo(&nested_path, "release");
        commit_all(&nested, "child v1");
        drop(nested);
        let detected = detect_repo(&nested_path).unwrap();
        assert_eq!(detected.root, nested_path);
        assert_eq!(detected.branch, "release");
        let detected_parent = detect_repo(&root).unwrap();
        assert_eq!(detected_parent.root, root);
        assert_eq!(detected_parent.branch, "main");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn detect_repo_from_a_nested_repo_subdirectory_returns_the_nested_repo() {
        let root = temp_dir("nested-subdir");
        let parent = init_repo(&root, "main");
        commit_all(&parent, "parent v1");
        drop(parent);
        let nested_path = root.join("vendor").join("child");
        fs::create_dir_all(nested_path.parent().unwrap()).unwrap();
        let nested = init_repo(&nested_path, "release");
        commit_all(&nested, "child v1");
        drop(nested);
        let subdir = nested_path.join("src").join("deep");
        fs::create_dir_all(&subdir).unwrap();
        let detected = detect_repo(&subdir).unwrap();
        assert_eq!(detected.root, nested_path);
        assert_eq!(detected.branch, "release");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn detect_repo_at_a_submodule_root_returns_the_submodule() {
        let root = temp_dir("submodule-root");
        let parent = init_repo(&root, "main");
        commit_all(&parent, "parent v1");
        drop(parent);
        let staging = temp_dir("submodule-staging");
        let child = init_repo(&staging, "release");
        commit_all(&child, "child v1");
        drop(child);
        let modules_dir = root.join(".git").join("modules");
        fs::create_dir_all(&modules_dir).unwrap();
        fs::rename(staging.join(".git"), modules_dir.join("child")).unwrap();
        let gitdir_config = modules_dir.join("child").join("config");
        let config_text = fs::read_to_string(&gitdir_config).unwrap();
        let sub_path = root.join("sub");
        fs::create_dir_all(&sub_path).unwrap();
        fs::write(
            &gitdir_config,
            format!(
                "{}\n[core]\n\tworktree = {}\n",
                config_text.trim_end(),
                sub_path.display()
            ),
        )
        .unwrap();
        fs::write(
            sub_path.join(".git"),
            format!("gitdir: {}\n", modules_dir.join("child").display()),
        )
        .unwrap();
        let detected = detect_repo(&sub_path).unwrap();
        assert_eq!(detected.root, sub_path);
        assert_eq!(detected.branch, "release");
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&staging);
    }

    #[test]
    fn detect_repo_uses_the_only_remote_when_it_is_not_origin() {
        let root = temp_dir("sole-remote");
        let repo = init_repo(&root, "main");
        commit_all(&repo, "v1");
        repo.remote("upstream", "https://example.com/repo.git")
            .unwrap();
        drop(repo);
        let detected = detect_repo(&root).unwrap();
        assert_eq!(detected.remote, "upstream");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn detect_repo_prefers_origin_over_other_remotes() {
        let root = temp_dir("origin-preferred");
        let repo = init_repo(&root, "main");
        commit_all(&repo, "v1");
        repo.remote("upstream", "https://example.com/repo.git")
            .unwrap();
        repo.remote("origin", "https://example.com/other.git")
            .unwrap();
        drop(repo);
        let detected = detect_repo(&root).unwrap();
        assert_eq!(detected.remote, "origin");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn detect_repo_rejects_a_path_outside_any_repo() {
        let root = temp_dir("not-a-repo");
        let err = detect_repo(&root).unwrap_err();
        assert!(
            format!("{:?}", err).contains("is not a valid git repository"),
            "unexpected error: {:?}",
            err
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn detect_repo_rejects_a_detached_head() {
        let root = temp_dir("detached");
        let repo = init_repo(&root, "main");
        let oid = commit_all(&repo, "v1");
        repo.set_head_detached(oid).unwrap();
        drop(repo);
        let err = detect_repo(&root).unwrap_err();
        assert!(
            format!("{:?}", err).contains("detached HEAD"),
            "unexpected error: {:?}",
            err
        );
        let _ = fs::remove_dir_all(&root);
    }
}
