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
    pub remote_branch: String,
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
    let branch = match repo.head() {
        Ok(head) => {
            if !head.is_branch() {
                return Err(ZlorbError::Other(format!(
                    "{} has a detached HEAD; check out the branch zlorb should watch and add again",
                    root.display()
                )));
            }
            head.shorthand()
                .ok_or_else(|| {
                    ZlorbError::Other(format!(
                        "could not read the current branch of {}",
                        root.display()
                    ))
                })?
                .to_string()
        }
        Err(e) if e.code() == git2::ErrorCode::UnbornBranch => {
            let head_ref = repo.find_reference("HEAD").map_err(ZlorbError::Git)?;
            let target = head_ref.symbolic_target().ok_or_else(|| {
                ZlorbError::Other(format!(
                    "could not read the current branch of {}",
                    root.display()
                ))
            })?;
            target
                .strip_prefix("refs/heads/")
                .filter(|name| !name.is_empty())
                .ok_or_else(|| {
                    ZlorbError::Other(format!(
                        "could not read the current branch of {}",
                        root.display()
                    ))
                })?
                .to_string()
        }
        Err(e) => return Err(ZlorbError::Git(e)),
    };
    let remotes = repo.remotes().map_err(ZlorbError::Git)?;
    let names: Vec<&str> = remotes.iter().flatten().collect();
    if let Some((upstream_remote, tracked_branch)) = upstream_target(&repo, &branch, &root)? {
        if !names.contains(&upstream_remote.as_str()) {
            return Err(ZlorbError::Other(format!(
                "{} branch {} tracks {} via remote {}, but that remote does not exist; add it with 'git remote add {} <url>' or fix the branch upstream with 'git branch --unset-upstream {}' and add again",
                root.display(),
                branch,
                tracked_branch,
                upstream_remote,
                upstream_remote,
                branch
            )));
        }
        return Ok(DetectedRepo {
            root,
            branch,
            remote: upstream_remote,
            remote_branch: tracked_branch,
        });
    }
    let remote = if names.contains(&"origin") {
        "origin".to_string()
    } else if names.len() == 1 {
        names[0].to_string()
    } else if names.is_empty() {
        return Err(ZlorbError::Other(format!(
            "{} has no git remote; add one with 'git remote add origin <url>' in that repo and add again",
            root.display()
        )));
    } else {
        return Err(ZlorbError::Other(format!(
            "{} has multiple git remotes ({}) but none named origin; rename the remote zlorb should track with 'git remote rename <name> origin' and add again",
            root.display(),
            names.join(", ")
        )));
    };
    Ok(DetectedRepo {
        root: root.clone(),
        branch: branch.clone(),
        remote,
        remote_branch: branch,
    })
}

fn upstream_target(
    repo: &Repository,
    branch: &str,
    root: &Path,
) -> Result<Option<(String, String)>, ZlorbError> {
    let config = repo.config().map_err(ZlorbError::Git)?;
    let remote = match config.get_string(&format!("branch.{}.remote", branch)) {
        Ok(value) => value,
        Err(e) if e.code() == git2::ErrorCode::NotFound => return Ok(None),
        Err(e) => return Err(ZlorbError::Git(e)),
    };
    if remote.is_empty() || remote == "." {
        return Ok(None);
    }
    if remote.starts_with('/')
        || remote.starts_with('.')
        || remote.starts_with('~')
        || remote.contains("://")
        || remote.contains(':')
    {
        return Err(ZlorbError::Other(format!(
            "{} branch {} tracks {}, which is a path or URL, not a named git remote; zlorb can only watch a named remote, so add one with 'git remote add <name> <url>', point the branch at it with 'git branch --set-upstream-to=<name>/<branch> {}', and add again",
            root.display(),
            branch,
            remote,
            branch
        )));
    }
    let merge = match config.get_string(&format!("branch.{}.merge", branch)) {
        Ok(value) => value,
        Err(e) if e.code() == git2::ErrorCode::NotFound => return Ok(None),
        Err(e) => return Err(ZlorbError::Git(e)),
    };
    let Some(tracked) = merge.strip_prefix("refs/heads/") else {
        return Ok(None);
    };
    if tracked.is_empty() {
        return Ok(None);
    }
    if !git2::Reference::is_valid_name(&format!("refs/heads/{}", tracked)) {
        return Err(ZlorbError::Other(format!(
            "{} branch {} has an upstream merge ref {} that is not a valid branch name; fix it with 'git branch --unset-upstream {}' or set a valid upstream and add again",
            root.display(),
            branch,
            merge,
            branch
        )));
    }
    Ok(Some((remote, tracked.to_string())))
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

    fn add_origin(repo: &Repository) {
        repo.remote("origin", "https://example.com/repo.git").unwrap();
    }

    fn set_upstream(repo: &Repository, branch: &str, remote: &str, merge: &str) {
        let mut config = repo.config().unwrap();
        config
            .set_str(&format!("branch.{}.remote", branch), remote)
            .unwrap();
        config
            .set_str(
                &format!("branch.{}.merge", branch),
                &format!("refs/heads/{}", merge),
            )
            .unwrap();
    }

    fn checkout_branch(repo: &Repository, branch: &str) {
        let oid = repo.head().unwrap().target().unwrap();
        let commit = repo.find_commit(oid).unwrap();
        repo.branch(branch, &commit, false).unwrap();
        drop(commit);
        repo.set_head(&format!("refs/heads/{}", branch)).unwrap();
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
        add_origin(&repo);
        drop(repo);
        let detected = detect_repo(&root).unwrap();
        assert_eq!(detected.branch, "main");
        assert_eq!(detected.root, root);
        assert_eq!(detected.remote, "origin");
        assert_eq!(detected.remote_branch, "main");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn detect_repo_reads_the_master_branch() {
        let root = temp_dir("master-branch");
        let repo = init_repo(&root, "master");
        commit_all(&repo, "v1");
        add_origin(&repo);
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
        add_origin(&repo);
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
        add_origin(&repo);
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
        add_origin(&parent);
        drop(parent);
        let nested_path = root.join("vendor").join("child");
        fs::create_dir_all(nested_path.parent().unwrap()).unwrap();
        let nested = init_repo(&nested_path, "release");
        commit_all(&nested, "child v1");
        add_origin(&nested);
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
        add_origin(&nested);
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
        add_origin(&child);
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
    fn detect_repo_honours_the_branch_upstream_over_origin() {
        let root = temp_dir("upstream-fork");
        let repo = init_repo(&root, "main");
        commit_all(&repo, "v1");
        checkout_branch(&repo, "dev");
        repo.remote("origin", "https://example.com/fork.git").unwrap();
        repo.remote("upstream", "https://example.com/repo.git")
            .unwrap();
        set_upstream(&repo, "dev", "upstream", "main");
        drop(repo);
        let detected = detect_repo(&root).unwrap();
        assert_eq!(detected.remote, "upstream");
        assert_eq!(detected.branch, "dev");
        assert_eq!(detected.remote_branch, "main");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn detect_repo_honours_an_upstream_with_the_same_branch_name() {
        let root = temp_dir("upstream-same-name");
        let repo = init_repo(&root, "main");
        commit_all(&repo, "v1");
        checkout_branch(&repo, "dev");
        add_origin(&repo);
        set_upstream(&repo, "dev", "origin", "dev");
        drop(repo);
        let detected = detect_repo(&root).unwrap();
        assert_eq!(detected.remote, "origin");
        assert_eq!(detected.branch, "dev");
        assert_eq!(detected.remote_branch, "dev");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn detect_repo_honours_a_nested_upstream_merge_branch() {
        let root = temp_dir("upstream-nested-merge");
        let repo = init_repo(&root, "main");
        commit_all(&repo, "v1");
        checkout_branch(&repo, "work");
        add_origin(&repo);
        set_upstream(&repo, "work", "origin", "feature/foo");
        drop(repo);
        let detected = detect_repo(&root).unwrap();
        assert_eq!(detected.remote, "origin");
        assert_eq!(detected.branch, "work");
        assert_eq!(detected.remote_branch, "feature/foo");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn detect_repo_rejects_an_upstream_remote_that_does_not_exist() {
        let root = temp_dir("upstream-missing-remote");
        let repo = init_repo(&root, "main");
        commit_all(&repo, "v1");
        add_origin(&repo);
        set_upstream(&repo, "main", "ghost", "main");
        drop(repo);
        let err = detect_repo(&root).unwrap_err();
        let text = format!("{:?}", err);
        assert!(
            text.contains("ghost") && text.contains("does not exist"),
            "unexpected error: {:?}",
            err
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn detect_repo_ignores_a_local_upstream() {
        let root = temp_dir("upstream-local");
        let repo = init_repo(&root, "main");
        commit_all(&repo, "v1");
        checkout_branch(&repo, "dev");
        add_origin(&repo);
        set_upstream(&repo, "dev", ".", "main");
        drop(repo);
        let detected = detect_repo(&root).unwrap();
        assert_eq!(detected.remote, "origin");
        assert_eq!(detected.branch, "dev");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn detect_repo_ignores_an_incomplete_upstream() {
        let root = temp_dir("upstream-incomplete");
        let repo = init_repo(&root, "main");
        commit_all(&repo, "v1");
        checkout_branch(&repo, "dev");
        repo.remote("upstream", "https://example.com/repo.git")
            .unwrap();
        add_origin(&repo);
        repo.config()
            .unwrap()
            .set_str("branch.dev.remote", "upstream")
            .unwrap();
        drop(repo);
        let detected = detect_repo(&root).unwrap();
        assert_eq!(detected.remote, "origin");
        assert_eq!(detected.branch, "dev");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn detect_repo_ignores_an_upstream_merge_outside_heads() {
        let root = temp_dir("upstream-merge-outside-heads");
        let repo = init_repo(&root, "main");
        commit_all(&repo, "v1");
        add_origin(&repo);
        let mut config = repo.config().unwrap();
        config.set_str("branch.main.remote", "origin").unwrap();
        config
            .set_str("branch.main.merge", "refs/remotes/origin/main")
            .unwrap();
        drop(config);
        drop(repo);
        let detected = detect_repo(&root).unwrap();
        assert_eq!(detected.remote, "origin");
        assert_eq!(detected.branch, "main");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn detect_repo_honours_the_upstream_of_an_unborn_head() {
        let root = temp_dir("unborn-upstream");
        let repo = init_repo(&root, "main");
        repo.remote("upstream", "https://example.com/repo.git")
            .unwrap();
        set_upstream(&repo, "main", "upstream", "trunk");
        drop(repo);
        let detected = detect_repo(&root).unwrap();
        assert_eq!(detected.remote, "upstream");
        assert_eq!(detected.branch, "main");
        assert_eq!(detected.remote_branch, "trunk");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn detect_repo_ignores_an_upstream_set_on_another_branch() {
        let root = temp_dir("upstream-other-branch");
        let repo = init_repo(&root, "main");
        commit_all(&repo, "v1");
        checkout_branch(&repo, "dev");
        repo.remote("upstream", "https://example.com/repo.git")
            .unwrap();
        add_origin(&repo);
        set_upstream(&repo, "main", "upstream", "main");
        drop(repo);
        let detected = detect_repo(&root).unwrap();
        assert_eq!(detected.remote, "origin");
        assert_eq!(detected.branch, "dev");
        assert_eq!(detected.remote_branch, "dev");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn detect_repo_honours_an_upstream_on_a_dotted_branch() {
        let root = temp_dir("upstream-dotted-branch");
        let repo = init_repo(&root, "main");
        commit_all(&repo, "v1");
        checkout_branch(&repo, "release/1.2");
        repo.remote("upstream", "https://example.com/repo.git")
            .unwrap();
        add_origin(&repo);
        drop(repo);
        let config_path = root.join(".git").join("config");
        let mut text = fs::read_to_string(&config_path).unwrap();
        text.push_str("[branch \"release/1.2\"]\n\tremote = upstream\n\tmerge = refs/heads/main\n");
        fs::write(&config_path, text).unwrap();
        let detected = detect_repo(&root).unwrap();
        assert_eq!(detected.remote, "upstream");
        assert_eq!(detected.branch, "release/1.2");
        assert_eq!(detected.remote_branch, "main");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn detect_repo_rejects_an_upstream_merge_that_is_not_a_valid_branch() {
        let root = temp_dir("upstream-invalid-merge");
        let repo = init_repo(&root, "main");
        commit_all(&repo, "v1");
        add_origin(&repo);
        let mut config = repo.config().unwrap();
        config.set_str("branch.main.remote", "origin").unwrap();
        config
            .set_str("branch.main.merge", "refs/heads/foo..bar")
            .unwrap();
        drop(config);
        drop(repo);
        let err = detect_repo(&root).unwrap_err();
        assert!(
            format!("{:?}", err).contains("not a valid branch name"),
            "unexpected error: {:?}",
            err
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn detect_repo_rejects_a_path_upstream_as_unsupported() {
        let root = temp_dir("upstream-path");
        let repo = init_repo(&root, "main");
        commit_all(&repo, "v1");
        add_origin(&repo);
        set_upstream(&repo, "main", "/srv/repo", "main");
        drop(repo);
        let err = detect_repo(&root).unwrap_err();
        let text = format!("{:?}", err);
        assert!(
            text.contains("path or URL") && text.contains("named git remote"),
            "unexpected error: {:?}",
            err
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn detect_repo_rejects_a_url_upstream_as_unsupported() {
        let root = temp_dir("upstream-url");
        let repo = init_repo(&root, "main");
        commit_all(&repo, "v1");
        add_origin(&repo);
        set_upstream(&repo, "main", "https://example.com/repo.git", "main");
        drop(repo);
        let err = detect_repo(&root).unwrap_err();
        assert!(
            format!("{:?}", err).contains("path or URL"),
            "unexpected error: {:?}",
            err
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn detect_repo_rejects_a_repo_with_no_remotes() {
        let root = temp_dir("no-remote");
        let repo = init_repo(&root, "main");
        commit_all(&repo, "v1");
        drop(repo);
        let err = detect_repo(&root).unwrap_err();
        assert!(
            format!("{:?}", err).contains("has no git remote"),
            "unexpected error: {:?}",
            err
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn detect_repo_rejects_multiple_remotes_without_origin() {
        let root = temp_dir("multi-no-origin");
        let repo = init_repo(&root, "main");
        commit_all(&repo, "v1");
        repo.remote("upstream", "https://example.com/repo.git")
            .unwrap();
        repo.remote("fork", "https://example.com/fork.git").unwrap();
        drop(repo);
        let err = detect_repo(&root).unwrap_err();
        let text = format!("{:?}", err);
        assert!(
            text.contains("multiple git remotes")
                && text.contains("upstream")
                && text.contains("fork"),
            "unexpected error: {:?}",
            err
        );
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
    fn detect_repo_reads_the_branch_of_an_unborn_head() {
        let root = temp_dir("unborn");
        let repo = init_repo(&root, "main");
        add_origin(&repo);
        drop(repo);
        let detected = detect_repo(&root).unwrap();
        assert_eq!(detected.branch, "main");
        assert_eq!(detected.root, root);
        assert_eq!(detected.remote, "origin");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn detect_repo_reads_a_non_initial_branch_of_an_unborn_head() {
        let root = temp_dir("unborn-non-initial");
        let repo = init_repo(&root, "main");
        repo.set_head("refs/heads/trunk").unwrap();
        add_origin(&repo);
        drop(repo);
        let detected = detect_repo(&root).unwrap();
        assert_eq!(detected.branch, "trunk");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn detect_repo_reads_a_nested_branch_of_an_unborn_head() {
        let root = temp_dir("unborn-nested-branch");
        let repo = init_repo(&root, "main");
        repo.set_head("refs/heads/feature/foo").unwrap();
        add_origin(&repo);
        drop(repo);
        let detected = detect_repo(&root).unwrap();
        assert_eq!(detected.branch, "feature/foo");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn detect_repo_rejects_an_unborn_head_with_no_remote() {
        let root = temp_dir("unborn-no-remote");
        let repo = init_repo(&root, "main");
        drop(repo);
        let err = detect_repo(&root).unwrap_err();
        assert!(
            format!("{:?}", err).contains("has no git remote"),
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
