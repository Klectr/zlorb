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
