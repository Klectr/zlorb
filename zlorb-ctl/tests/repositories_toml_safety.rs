use git2::{IndexAddOption, RepositoryInitOptions, Signature};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_dir(label: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "zlorb-ctl-safety-test-{}-{}-{}",
        std::process::id(),
        label,
        n
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn init_repo(path: &Path, branch: &str) {
    let mut opts = RepositoryInitOptions::new();
    opts.initial_head(branch);
    let repo = git2::Repository::init_opts(path, &opts).unwrap();
    std::fs::write(path.join("file.txt"), "v1").unwrap();
    let mut index = repo.index().unwrap();
    index
        .add_all(["*"].iter(), IndexAddOption::DEFAULT, None)
        .unwrap();
    index.write().unwrap();
    let tree_oid = index.write_tree().unwrap();
    let tree = repo.find_tree(tree_oid).unwrap();
    let sig = Signature::now("zlorb-test", "zlorb-test@example.com").unwrap();
    repo.commit(Some("HEAD"), &sig, &sig, "v1", &tree, &[])
        .unwrap();
    repo.remote("origin", "https://example.com/repo.git")
        .unwrap();
}

fn run_ctl(args: &[&str], cwd: &Path, config_dir: &Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_zlorb-ctl"))
        .args(args)
        .current_dir(cwd)
        .env("ZLORB_CONFIG_DIR", config_dir)
        .output()
        .unwrap()
}

fn repos_file(config_dir: &Path) -> PathBuf {
    config_dir.join("repositories.toml")
}

#[test]
fn duplicate_add_succeeds_and_leaves_repositories_file_byte_identical() {
    let root = temp_dir("duplicate-add");
    let config_dir = root.join("config");
    let repo_path = root.join("my-app");
    init_repo(&repo_path, "main");

    let first = run_ctl(&["add"], &repo_path, &config_dir);
    assert!(
        first.status.success(),
        "first add failed: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    let before = std::fs::read(repos_file(&config_dir)).unwrap();
    assert!(!before.is_empty());

    let second = run_ctl(&["add"], &repo_path, &config_dir);
    assert!(
        second.status.success(),
        "duplicate add should be a no-op success, got: {}",
        String::from_utf8_lossy(&second.stderr)
    );
    let after = std::fs::read(repos_file(&config_dir)).unwrap();
    assert_eq!(before, after, "duplicate add must not modify repositories.toml");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn remove_of_unknown_repo_succeeds_and_leaves_repositories_file_byte_identical() {
    let root = temp_dir("unknown-remove");
    let config_dir = root.join("config");
    let repo_path = root.join("my-app");
    init_repo(&repo_path, "main");

    let added = run_ctl(&["add"], &repo_path, &config_dir);
    assert!(added.status.success());
    let before = std::fs::read(repos_file(&config_dir)).unwrap();
    assert!(!before.is_empty());

    let removed = run_ctl(&["remove", "--repo-name", "no-such-repo"], &repo_path, &config_dir);
    assert!(
        removed.status.success(),
        "remove of unknown repo should be a no-op success, got: {}",
        String::from_utf8_lossy(&removed.stderr)
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&removed.stdout),
        String::from_utf8_lossy(&removed.stderr)
    );
    assert!(
        combined.contains("repo 'no-such-repo' is not tracked"),
        "no-op remove should name the untracked repo, got: {}",
        combined
    );
    let after = std::fs::read(repos_file(&config_dir)).unwrap();
    assert_eq!(
        before, after,
        "no-op remove must not modify repositories.toml"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn add_and_remove_against_malformed_repositories_file_fail_and_leave_it_unchanged() {
    let root = temp_dir("malformed");
    let config_dir = root.join("config");
    std::fs::create_dir_all(&config_dir).unwrap();
    let repo_path = root.join("my-app");
    init_repo(&repo_path, "main");
    let bad = "this is not = valid = toml [[\n";
    std::fs::write(repos_file(&config_dir), bad).unwrap();

    let added = run_ctl(&["add"], &repo_path, &config_dir);
    assert!(
        !added.status.success(),
        "add against a malformed repositories.toml unexpectedly succeeded"
    );
    let added_output = format!(
        "{}{}",
        String::from_utf8_lossy(&added.stdout),
        String::from_utf8_lossy(&added.stderr)
    );
    assert!(
        added_output.contains("repositories.toml"),
        "add error should name the config file, got: {}",
        added_output
    );
    assert!(
        added_output.contains("Failed to parse config"),
        "add error should report the parse failure, got: {}",
        added_output
    );
    assert_eq!(
        std::fs::read_to_string(repos_file(&config_dir)).unwrap(),
        bad,
        "failed add must not overwrite a malformed repositories.toml"
    );

    let removed = run_ctl(&["remove", "--repo-name", "my-app"], &repo_path, &config_dir);
    assert!(
        !removed.status.success(),
        "remove against a malformed repositories.toml unexpectedly succeeded"
    );
    assert_eq!(
        std::fs::read_to_string(repos_file(&config_dir)).unwrap(),
        bad,
        "failed remove must not overwrite a malformed repositories.toml"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn remove_of_watched_repo_succeeds_and_updates_the_file() {
    let root = temp_dir("remove-ok");
    let config_dir = root.join("config");
    let repo_path = root.join("my-app");
    init_repo(&repo_path, "main");

    let added = run_ctl(&["add"], &repo_path, &config_dir);
    assert!(added.status.success());
    assert!(
        std::fs::read_to_string(repos_file(&config_dir))
            .unwrap()
            .contains("my-app")
    );

    let removed = run_ctl(&["remove", "--repo-name", "my-app"], &repo_path, &config_dir);
    assert!(
        removed.status.success(),
        "remove failed: {}",
        String::from_utf8_lossy(&removed.stderr)
    );
    assert!(
        !std::fs::read_to_string(repos_file(&config_dir))
            .unwrap()
            .contains("my-app"),
        "removed repo still present in repositories.toml"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn second_remove_of_the_same_repo_is_a_no_op_success() {
    let root = temp_dir("remove-twice");
    let config_dir = root.join("config");
    let repo_path = root.join("my-app");
    init_repo(&repo_path, "main");

    let added = run_ctl(&["add"], &repo_path, &config_dir);
    assert!(added.status.success());

    let removed = run_ctl(&["remove", "--repo-name", "my-app"], &repo_path, &config_dir);
    assert!(removed.status.success());
    let before = std::fs::read(repos_file(&config_dir)).unwrap();

    let removed_again = run_ctl(&["remove", "--repo-name", "my-app"], &repo_path, &config_dir);
    assert!(
        removed_again.status.success(),
        "second remove should be a no-op success, got: {}",
        String::from_utf8_lossy(&removed_again.stderr)
    );
    let after = std::fs::read(repos_file(&config_dir)).unwrap();
    assert_eq!(before, after, "second remove must not modify repositories.toml");
    let _ = std::fs::remove_dir_all(&root);
}
