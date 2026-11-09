use git2::{IndexAddOption, RepositoryInitOptions, Signature};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_dir(label: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "zlorb-ctl-add-test-{}-{}-{}",
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
}

fn run_add(cwd: &Path, config_dir: &Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_zlorb-ctl"))
        .arg("add")
        .current_dir(cwd)
        .env("ZLORB_CONFIG_DIR", config_dir)
        .output()
        .unwrap()
}

#[test]
fn add_records_the_repos_branch_remote_and_root() {
    let root = temp_dir("main-repo");
    let config_dir = root.join("config");
    let repo_path = root.join("my-app");
    init_repo(&repo_path, "main");
    let repo = git2::Repository::open(&repo_path).unwrap();
    repo.remote("upstream", "https://example.com/repo.git")
        .unwrap();
    drop(repo);
    let subdir = repo_path.join("src").join("deep");
    std::fs::create_dir_all(&subdir).unwrap();

    let output = run_add(&subdir, &config_dir);
    assert!(
        output.status.success(),
        "zlorb-ctl add failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let repos = std::fs::read_to_string(config_dir.join("repositories.toml")).unwrap();
    assert!(
        repos.contains("name = \"my-app\""),
        "name missing: {}",
        repos
    );
    assert!(
        repos.contains(&format!("path = \"{}\"", repo_path.display())),
        "root path missing: {}",
        repos
    );
    assert!(
        repos.contains("branch = \"main\""),
        "detected branch missing: {}",
        repos
    );
    assert!(
        repos.contains("remote = \"upstream\""),
        "detected remote missing: {}",
        repos
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn add_records_the_branch_upstream_when_configured() {
    let root = temp_dir("upstream-repo");
    let config_dir = root.join("config");
    let repo_path = root.join("my-app");
    init_repo(&repo_path, "main");
    let repo = git2::Repository::open(&repo_path).unwrap();
    let oid = repo.head().unwrap().target().unwrap();
    let commit = repo.find_commit(oid).unwrap();
    repo.branch("dev", &commit, false).unwrap();
    drop(commit);
    repo.set_head("refs/heads/dev").unwrap();
    repo.remote("origin", "https://example.com/fork.git").unwrap();
    repo.remote("upstream", "https://example.com/repo.git")
        .unwrap();
    let mut config = repo.config().unwrap();
    config.set_str("branch.dev.remote", "upstream").unwrap();
    config
        .set_str("branch.dev.merge", "refs/heads/main")
        .unwrap();
    drop(config);
    drop(repo);

    let output = run_add(&repo_path, &config_dir);
    assert!(
        output.status.success(),
        "zlorb-ctl add failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let repos = std::fs::read_to_string(config_dir.join("repositories.toml")).unwrap();
    assert!(
        repos.contains("branch = \"dev\""),
        "local branch missing: {}",
        repos
    );
    assert!(
        repos.contains("remote_branch = \"main\""),
        "tracked branch missing: {}",
        repos
    );
    assert!(
        repos.contains("remote = \"upstream\""),
        "upstream remote missing: {}",
        repos
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn add_without_a_remote_fails_with_an_actionable_message() {
    let root = temp_dir("no-remote");
    let config_dir = root.join("config");
    let repo_path = root.join("my-app");
    init_repo(&repo_path, "main");
    let output = run_add(&repo_path, &config_dir);
    assert!(
        !output.status.success(),
        "zlorb-ctl add unexpectedly succeeded for a repo with no remote"
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains("has no git remote"),
        "add error should explain the missing remote, got: {}",
        combined
    );
    assert!(
        !config_dir.join("repositories.toml").exists(),
        "failed add must not create repositories.toml"
    );
    let _ = std::fs::remove_dir_all(&root);
}

fn init_repo_with_origin(path: &Path, branch: &str) {
    init_repo(path, branch);
    let repo = git2::Repository::open(path).unwrap();
    repo.remote("origin", "https://example.com/repo.git")
        .unwrap();
}

#[test]
fn add_same_repo_twice_is_a_no_op_success() {
    let root = temp_dir("duplicate-path");
    let config_dir = root.join("config");
    let repo_path = root.join("my-app");
    init_repo_with_origin(&repo_path, "main");
    let subdir = repo_path.join("src");
    std::fs::create_dir_all(&subdir).unwrap();

    let first = run_add(&repo_path, &config_dir);
    assert!(
        first.status.success(),
        "first add failed: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    let second = run_add(&subdir, &config_dir);
    assert!(
        second.status.success(),
        "second add of the same repo should be a no-op success, got: {}",
        String::from_utf8_lossy(&second.stderr)
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&second.stdout),
        String::from_utf8_lossy(&second.stderr)
    );
    assert!(
        combined.contains("already in the configuration as 'my-app'"),
        "duplicate add should name the tracked name, got: {}",
        combined
    );
    assert!(
        combined.contains(&repo_path.display().to_string()),
        "duplicate add should name the path, got: {}",
        combined
    );
    let repos = std::fs::read_to_string(config_dir.join("repositories.toml")).unwrap();
    assert_eq!(
        repos.matches("[[repository]]").count(),
        1,
        "duplicate add must leave exactly one entry: {}",
        repos
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn add_is_a_no_op_success_when_a_seeded_entry_aliases_the_same_path() {
    let root = temp_dir("duplicate-alias");
    let config_dir = root.join("config");
    let repo_path = root.join("my-app");
    init_repo_with_origin(&repo_path, "main");
    std::fs::create_dir_all(&config_dir).unwrap();
    let alias = format!("{}/./", repo_path.display());
    std::fs::write(
        config_dir.join("repositories.toml"),
        format!(
            "[[repository]]\nname = \"seeded-app\"\npath = \"{}\"\nremote = \"origin\"\nbranch = \"main\"\n",
            alias
        ),
    )
    .unwrap();

    let output = run_add(&repo_path, &config_dir);
    assert!(
        output.status.success(),
        "add of a canonically identical path should be a no-op success, got: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains("already in the configuration as 'seeded-app'"),
        "alias duplicate add should name the tracked name, got: {}",
        combined
    );
    let repos = std::fs::read_to_string(config_dir.join("repositories.toml")).unwrap();
    assert_eq!(
        repos.matches("[[repository]]").count(),
        1,
        "alias duplicate must leave exactly one entry: {}",
        repos
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn add_two_repos_sharing_a_basename_tracks_both_under_distinct_names() {
    let root = temp_dir("same-basename");
    let config_dir = root.join("config");
    let first_path = root.join("work").join("app");
    let second_path = root.join("personal").join("app");
    std::fs::create_dir_all(first_path.parent().unwrap()).unwrap();
    std::fs::create_dir_all(second_path.parent().unwrap()).unwrap();
    init_repo_with_origin(&first_path, "main");
    init_repo_with_origin(&second_path, "main");

    let first = run_add(&first_path, &config_dir);
    assert!(
        first.status.success(),
        "first add failed: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    let second = run_add(&second_path, &config_dir);
    assert!(
        second.status.success(),
        "second same-basename add failed: {}{}",
        String::from_utf8_lossy(&second.stdout),
        String::from_utf8_lossy(&second.stderr)
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&second.stdout),
        String::from_utf8_lossy(&second.stderr)
    );
    assert!(
        combined.contains("personal-app"),
        "second add should report its disambiguated name, got: {}",
        combined
    );
    let repos = std::fs::read_to_string(config_dir.join("repositories.toml")).unwrap();
    assert_eq!(
        repos.matches("[[repository]]").count(),
        2,
        "both repos should be tracked: {}",
        repos
    );
    assert!(
        repos.contains("name = \"app\""),
        "first name missing: {}",
        repos
    );
    assert!(
        repos.contains("name = \"personal-app\""),
        "disambiguated name missing: {}",
        repos
    );
    assert!(
        repos.contains(&format!("path = \"{}\"", first_path.display())),
        "first path missing: {}",
        repos
    );
    assert!(
        repos.contains(&format!("path = \"{}\"", second_path.display())),
        "second path missing: {}",
        repos
    );
    let second_block = repos
        .split("[[repository]]")
        .find(|block| block.contains(&second_path.display().to_string()))
        .expect("second repo entry missing");
    assert!(
        second_block.contains("name = \"personal-app\""),
        "personal-app must be the name of the second path's entry: {}",
        second_block
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn add_disambiguation_falls_back_to_a_numeric_suffix() {
    let root = temp_dir("same-basename-suffix");
    let config_dir = root.join("config");
    let first_path = root.join("a").join("personal").join("app");
    let third_path = root.join("c").join("personal").join("app");
    let fourth_path = root.join("d").join("personal").join("app");
    for path in [&first_path, &third_path, &fourth_path] {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        init_repo_with_origin(path, "main");
    }
    let base_path = root.join("base").join("app");
    std::fs::create_dir_all(base_path.parent().unwrap()).unwrap();
    init_repo_with_origin(&base_path, "main");

    assert!(run_add(&base_path, &config_dir).status.success());
    assert!(run_add(&first_path, &config_dir).status.success());
    let third = run_add(&third_path, &config_dir);
    assert!(
        third.status.success(),
        "third add failed: {}{}",
        String::from_utf8_lossy(&third.stdout),
        String::from_utf8_lossy(&third.stderr)
    );
    let fourth = run_add(&fourth_path, &config_dir);
    assert!(
        fourth.status.success(),
        "fourth add failed: {}{}",
        String::from_utf8_lossy(&fourth.stdout),
        String::from_utf8_lossy(&fourth.stderr)
    );
    let repos = std::fs::read_to_string(config_dir.join("repositories.toml")).unwrap();
    assert_eq!(
        repos.matches("[[repository]]").count(),
        4,
        "all four repos should be tracked: {}",
        repos
    );
    assert!(
        repos.contains("name = \"app\""),
        "base name missing: {}",
        repos
    );
    assert!(
        repos.contains("name = \"personal-app\""),
        "parent-qualified name missing: {}",
        repos
    );
    assert!(
        repos.contains("name = \"personal-app-2\""),
        "suffix fallback name missing: {}",
        repos
    );
    let fourth_block = repos
        .split("[[repository]]")
        .find(|block| block.contains(&fourth_path.display().to_string()))
        .expect("fourth repo entry missing");
    assert!(
        fourth_block.contains("name = \"personal-app-3\""),
        "fourth repo should continue the suffix chain: {}",
        fourth_block
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[cfg(unix)]
#[test]
fn add_a_repo_with_a_non_utf8_path_fails_and_writes_nothing() {
    use std::os::unix::ffi::OsStringExt;
    let root = temp_dir("non-utf8");
    let config_dir = root.join("config");
    let repo_path = root.join(std::ffi::OsString::from_vec(b"bad-\xff-app".to_vec()));
    std::fs::create_dir_all(&repo_path).unwrap();
    init_repo_with_origin(&repo_path, "main");

    let output = run_add(&repo_path, &config_dir);
    assert!(
        !output.status.success(),
        "zlorb-ctl add unexpectedly succeeded for a non-UTF-8 path"
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains("not valid UTF-8"),
        "add error should explain the UTF-8 problem, got: {}",
        combined
    );
    assert!(
        !config_dir.join("repositories.toml").exists(),
        "failed add must not create repositories.toml"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn add_outside_a_repo_preserves_the_underlying_git_error() {
    let root = temp_dir("not-a-repo-detail");
    let config_dir = root.join("config");
    let output = run_add(&root, &config_dir);
    assert!(
        !output.status.success(),
        "zlorb-ctl add unexpectedly succeeded outside a repo"
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains("is not a valid git repository")
            && combined.contains("could not find repository"),
        "add error should preserve the underlying git2 message, got: {}",
        combined
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn add_outside_a_repo_fails() {
    let root = temp_dir("not-a-repo");
    let config_dir = root.join("config");
    let output = run_add(&root, &config_dir);
    assert!(
        !output.status.success(),
        "zlorb-ctl add unexpectedly succeeded outside a repo"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn re_add_with_unchanged_detection_leaves_repositories_toml_byte_identical() {
    let root = temp_dir("re-add-unchanged");
    let config_dir = root.join("config");
    let repo_path = root.join("my-app");
    init_repo_with_origin(&repo_path, "main");

    let first = run_add(&repo_path, &config_dir);
    assert!(first.status.success());
    let before = std::fs::read(config_dir.join("repositories.toml")).unwrap();

    let second = run_add(&repo_path, &config_dir);
    assert!(
        second.status.success(),
        "unchanged re-add failed: {}",
        String::from_utf8_lossy(&second.stderr)
    );
    let after = std::fs::read(config_dir.join("repositories.toml")).unwrap();
    assert_eq!(
        before, after,
        "unchanged re-add must not rewrite repositories.toml"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn re_add_refreshes_branch_and_upstream_and_preserves_user_fields() {
    let root = temp_dir("re-add-refresh");
    let config_dir = root.join("config");
    let repo_path = root.join("my-app");
    init_repo_with_origin(&repo_path, "main");

    let first = run_add(&repo_path, &config_dir);
    assert!(
        first.status.success(),
        "first add failed: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    let mut seeded = std::fs::read_to_string(config_dir.join("repositories.toml")).unwrap();
    seeded.push_str("build_strategy = \"rust\"\nrestart_command = \"restart-app\"\n");
    std::fs::write(config_dir.join("repositories.toml"), seeded).unwrap();

    let repo = git2::Repository::open(&repo_path).unwrap();
    let oid = repo.head().unwrap().target().unwrap();
    let commit = repo.find_commit(oid).unwrap();
    repo.branch("dev", &commit, false).unwrap();
    drop(commit);
    repo.set_head("refs/heads/dev").unwrap();
    repo.remote("upstream", "https://example.com/repo.git")
        .unwrap();
    let mut git_config = repo.config().unwrap();
    git_config.set_str("branch.dev.remote", "upstream").unwrap();
    git_config
        .set_str("branch.dev.merge", "refs/heads/main")
        .unwrap();
    drop(git_config);
    drop(repo);

    let second = run_add(&repo_path, &config_dir);
    assert!(
        second.status.success(),
        "re-add failed: {}",
        String::from_utf8_lossy(&second.stderr)
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&second.stdout),
        String::from_utf8_lossy(&second.stderr)
    );
    assert!(
        combined.contains("already in the configuration as 'my-app'")
            && combined.contains("updated its tracked branch"),
        "re-add should report the refresh, got: {}",
        combined
    );
    let repos = std::fs::read_to_string(config_dir.join("repositories.toml")).unwrap();
    assert_eq!(
        repos.matches("[[repository]]").count(),
        1,
        "refresh must keep exactly one entry: {}",
        repos
    );
    assert!(
        repos.contains("branch = \"dev\""),
        "branch not refreshed: {}",
        repos
    );
    assert!(
        repos.contains("remote = \"upstream\""),
        "remote not refreshed: {}",
        repos
    );
    assert!(
        repos.contains("remote_branch = \"main\""),
        "remote_branch not refreshed: {}",
        repos
    );
    assert!(
        repos.contains("build_strategy = \"rust\""),
        "user build_strategy must survive the refresh: {}",
        repos
    );
    assert!(
        repos.contains("restart_command = \"restart-app\""),
        "user restart_command must survive the refresh: {}",
        repos
    );
    assert!(
        repos.contains(&format!("path = \"{}\"", repo_path.display())),
        "path must be unchanged: {}",
        repos
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn re_add_clears_a_stale_remote_branch_when_the_upstream_is_gone() {
    let root = temp_dir("re-add-clear-remote-branch");
    let config_dir = root.join("config");
    let repo_path = root.join("my-app");
    init_repo(&repo_path, "main");
    let repo = git2::Repository::open(&repo_path).unwrap();
    let oid = repo.head().unwrap().target().unwrap();
    let commit = repo.find_commit(oid).unwrap();
    repo.branch("dev", &commit, false).unwrap();
    drop(commit);
    repo.set_head("refs/heads/dev").unwrap();
    repo.remote("origin", "https://example.com/fork.git").unwrap();
    repo.remote("upstream", "https://example.com/repo.git")
        .unwrap();
    let mut git_config = repo.config().unwrap();
    git_config.set_str("branch.dev.remote", "upstream").unwrap();
    git_config
        .set_str("branch.dev.merge", "refs/heads/main")
        .unwrap();
    drop(git_config);
    drop(repo);

    let first = run_add(&repo_path, &config_dir);
    assert!(
        first.status.success(),
        "first add failed: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    let seeded = std::fs::read_to_string(config_dir.join("repositories.toml")).unwrap();
    assert!(
        seeded.contains("remote_branch = \"main\""),
        "first add should record the differing remote branch: {}",
        seeded
    );

    let repo = git2::Repository::open(&repo_path).unwrap();
    repo.set_head("refs/heads/main").unwrap();
    drop(repo);

    let second = run_add(&repo_path, &config_dir);
    assert!(
        second.status.success(),
        "re-add failed: {}",
        String::from_utf8_lossy(&second.stderr)
    );
    let repos = std::fs::read_to_string(config_dir.join("repositories.toml")).unwrap();
    assert!(
        repos.contains("branch = \"main\""),
        "branch not refreshed to main: {}",
        repos
    );
    assert!(
        repos.contains("remote = \"origin\""),
        "remote not refreshed to origin: {}",
        repos
    );
    assert!(
        !repos.contains("remote_branch"),
        "stale remote_branch must be cleared: {}",
        repos
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn re_add_of_a_tracked_repo_whose_remote_was_removed_is_a_no_op_success() {
    let root = temp_dir("tracked-remote-removed");
    let config_dir = root.join("config");
    let repo_path = root.join("my-app");
    init_repo_with_origin(&repo_path, "main");

    let first = run_add(&repo_path, &config_dir);
    assert!(
        first.status.success(),
        "first add failed: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    let before = std::fs::read(config_dir.join("repositories.toml")).unwrap();

    let repo = git2::Repository::open(&repo_path).unwrap();
    repo.remote_delete("origin").unwrap();
    drop(repo);

    let second = run_add(&repo_path, &config_dir);
    assert!(
        second.status.success(),
        "re-add of a tracked repo without a remote should be a no-op success, got: {}",
        String::from_utf8_lossy(&second.stderr)
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&second.stdout),
        String::from_utf8_lossy(&second.stderr)
    );
    assert!(
        combined.contains("already in the configuration as 'my-app'"),
        "no-op re-add should name the tracked name, got: {}",
        combined
    );
    let after = std::fs::read(config_dir.join("repositories.toml")).unwrap();
    assert_eq!(before, after, "no-op re-add must not modify repositories.toml");
    let _ = std::fs::remove_dir_all(&root);
}
